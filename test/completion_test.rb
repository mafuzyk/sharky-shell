# frozen_string_literal: true

require_relative "test_helper"
require "sharky"
require "stringio"
require "timeout"

class CompletionTest < Minitest::Test
  def setup
    super
    FileUtils.mkdir_p(File.join(@tmp, "bin"))
    File.write(File.join(@tmp, "bin", "echo"), "#!/bin/sh\n")
    FileUtils.chmod(0755, File.join(@tmp, "bin", "echo"))
    @runtime = Sharky::Runtime.new
    @runtime.aliases["ll"] = "printf ll"
    @runtime.abbreviations["gco"] = "git checkout"
    @runtime.commands["hello"] = ->(_argv) { 0 }
    @shell = Sharky::Shell.new(env: { "PATH" => File.join(@tmp, "bin"), "HOME" => @tmp }, cwd: @tmp)
    @completion = Sharky::Completion.new(runtime: @runtime, shell: @shell)
  end

  def test_command_position_completion
    values = @completion.candidates("ech", 3).map(&:value)
    assert_includes values, "echo"
    assert_includes @completion.candidates("l", 1).map(&:value), "ll"

    aliases = @completion.candidates("g", 1).map(&:value)
    assert_includes aliases, "gco"
    assert_includes @completion.candidates("h", 1).map(&:value), "hello"
    assert_includes @completion.candidates("c", 1).map(&:value), "cd"
  end

  def test_command_position_after_a_pipe
    assert_includes @completion.candidates("printf hi | ec", 13).map(&:value), "echo"
  end

  def test_compound_keywords_complete
    assert_includes @completion.candidates("th", 2).map(&:value), "then"
    assert_includes @completion.candidates("f", 1).map(&:value), "fi"
    assert_includes @completion.candidates("do", 2).map(&:value), "done"
  end

  def test_shell_function_completes
    @shell.functions["myfunc"] = Sharky::AST::List.new(entries: [])
    assert_includes @completion.candidates("myf", 3).map(&:value), "myfunc"
  end

  def test_array_names_complete_as_variables
    @shell.set_array("myarray", ["one"])
    assert_includes @completion.candidates("echo $mya", 9).map(&:value), "myarray"
  end

  def test_file_completion
    File.write(File.join(@tmp, "alpha.txt"), "")
    assert_includes @completion.candidates("cat al", 6).map(&:value), "alpha.txt"
  end

  def test_directory_and_tilde_file_completion
    FileUtils.mkdir_p(File.join(@tmp, "docs"))
    File.write(File.join(@tmp, "docs", "guide.txt"), "")
    File.write(File.join(@tmp, "notes.txt"), "")

    assert_includes @completion.candidates("cat do", 6).map(&:value), "docs/"
    assert_includes @completion.candidates("cat docs/gu", 11).map(&:value), "docs/guide.txt"
    assert_includes @completion.candidates("cat ~/no", 9).map(&:value), "~/notes.txt"
  end

  def test_git_branch_completion
    Dir.chdir(@tmp) do
      system("git", "init", "-q")
      system("git", "config", "user.email", "t@example.com")
      system("git", "config", "user.name", "T")
      File.write("a", "a")
      system("git", "add", "a")
      system("git", "commit", "-qm", "init")
      system("git", "branch", "feature-x")
    end
    values = @completion.candidates("git checkout fea", 17).map(&:value)
    assert_includes values, "feature-x"
  end

  def test_variable_completion
    assert_equal [6, "HO"], @completion.token("echo $HO", 8)
    assert_includes @completion.candidates("echo $HO", 8).map(&:value), "HOME"
  end

  def test_user_completion_proc
    @runtime.completions["cmd"] = ->(_word, _line, _cursor) { ["custom"] }
    assert_includes @completion.candidates("cmd c", 5).map(&:value), "custom"
  end

  def test_user_completion_proc_pairs_and_descriptions
    @runtime.completions["cmd"] = lambda do |word, _line, _cursor|
      [[word + "tom", "custom value"]]
    end
    candidate = @completion.candidates("cmd cus", 7).find { |item| item.value == "custom" }
    refute_nil candidate
    assert_equal "custom value", candidate.description
  end

  def test_candidate_descriptions
    alias_candidate = @completion.candidates("l", 1).find { |item| item.value == "ll" }
    builtin_candidate = @completion.candidates("c", 1).find { |item| item.value == "cd" }

    assert_equal "alias", alias_candidate.description
    assert_equal "builtin", builtin_candidate.description
  end
end

class CompletionEditorTest < Minitest::Test
  class FakeTerminal
    attr_reader :input, :output

    def initialize(input, output)
      @input = input
      @output = output
    end

    def tty?
      true
    end

    def with_raw
      yield
    end

    def width(text)
      text.to_s.length
    end

    def size
      [80, 24]
    end
  end

  def setup
    super
    @saved_no_color = ENV["NO_COLOR"]
    @saved_sharky_no_color = ENV["SHARKY_NO_COLOR"]
    ENV.delete("NO_COLOR")
    ENV.delete("SHARKY_NO_COLOR")
    FileUtils.mkdir_p(File.join(@tmp, "bin"))
    %w[echo].each do |name|
      path = File.join(@tmp, "bin", name)
      File.write(path, "#!/bin/sh\n")
      FileUtils.chmod(0755, path)
    end
    %w[alpha.txt alpine.txt].each { |name| File.write(File.join(@tmp, name), "") }
    @runtime = Sharky::Runtime.new
    @shell = Sharky::Shell.new(
      env: { "PATH" => File.join(@tmp, "bin"), "HOME" => @tmp },
      cwd: @tmp
    )
    @completion = Sharky::Completion.new(runtime: @runtime, shell: @shell)
  end

  def teardown
    ENV["NO_COLOR"] = @saved_no_color
    ENV["SHARKY_NO_COLOR"] = @saved_sharky_no_color
    super
  end

  def type_line(keys)
    reader, writer = IO.pipe
    output = StringIO.new
    terminal = FakeTerminal.new(reader, output)
    editor = Sharky::Editor.new(
      terminal: terminal,
      history: Sharky::History.new(path: File.join(@tmp, "history")),
      prompt: "> ",
      theme: Sharky::Theme.new,
      runtime: @runtime,
      completion: @completion,
      output: output
    )
    thread = Thread.new { editor.readline }
    writer.write(keys)
    writer.close
    Timeout.timeout(5) { thread.value }
  ensure
    thread&.kill if thread&.alive?
    writer&.close unless writer&.closed?
    reader&.close unless reader&.closed?
  end

  def test_tab_completes_a_unique_command_prefix
    assert_equal "echo", type_line("ech\t\r")
  end

  def test_tab_inserts_the_longest_common_prefix_and_escape_cancels_the_pager
    assert_equal "cat alp", type_line("cat al\t\e\r")
  end

  def test_shift_tab_opens_the_pager_on_the_last_candidate
    assert_equal "cat alpine.txt", type_line("cat al\e[Z\r\r")
  end

  def test_tab_cycles_forward_inside_the_pager
    assert_equal "cat alpine.txt", type_line("cat al\t\t\r\r")
  end

  def test_tab_does_nothing_without_candidates
    assert_equal "cat zz", type_line("cat zz\t\r")
  end
end
