# frozen_string_literal: true

require_relative "test_helper"
require "sharky"
require "stringio"
require "timeout"

class FuzzyTest < Minitest::Test
  def test_subsequence_and_boundary_scoring
    assert_operator Sharky::Fuzzy.score("gp", "git push"), :>, 0
    assert_operator Sharky::Fuzzy.score("gp", "grep -r pattern"), :>, 0
    assert_operator Sharky::Fuzzy.score("gp", "git push"), :>, Sharky::Fuzzy.score("gp", "grep -r pattern")
    assert_equal 0, Sharky::Fuzzy.score("zzz", "git push")
  end

  def test_case_insensitive
    assert_operator Sharky::Fuzzy.score("GP", "git push"), :>, 0
  end

  def test_file_scan_skips_hidden_directories_and_caps
    FileUtils.mkdir_p(File.join(@tmp, "src", "nested"))
    FileUtils.mkdir_p(File.join(@tmp, ".git"))
    FileUtils.mkdir_p(File.join(@tmp, ".hidden"))
    File.write(File.join(@tmp, "visible.txt"), "")
    File.write(File.join(@tmp, ".env"), "")
    File.write(File.join(@tmp, "src", "file.rb"), "")
    File.write(File.join(@tmp, ".git", "config"), "")
    File.write(File.join(@tmp, ".hidden", "secret.txt"), "")

    files, capped = Sharky::Fuzzy.files(@tmp)

    refute capped
    assert_includes files, "visible.txt"
    assert_includes files, ".env"
    assert_includes files, "src/"
    assert_includes files, "src/nested/"
    assert_includes files, "src/file.rb"
    refute(files.any? { |path| path.start_with?(".git/", ".hidden/") })

    limited, limited_capped = Sharky::Fuzzy.files(@tmp, limit: 2)
    assert_equal 2, limited.length
    assert limited_capped
  end
end

class FuzzyEditorTest < Minitest::Test
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
    @history = Sharky::History.new(path: File.join(@tmp, "history"))
    @history.add("git status").add("git push").add("ls")
    @runtime = Sharky::Runtime.new
    @shell = Sharky::Shell.new(env: { "HOME" => @tmp, "PATH" => ENV["PATH"] }, cwd: @tmp)
  end

  def type_line(keys)
    reader, writer = IO.pipe
    output = StringIO.new
    terminal = FakeTerminal.new(reader, output)
    editor = Sharky::Editor.new(
      terminal: terminal,
      history: @history,
      prompt: "> ",
      theme: Sharky::Theme.new,
      runtime: @runtime,
      output: output,
      shell: @shell
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

  def test_ctrl_r_fuzzy_search_executes_the_selected_command
    assert_equal "git push", type_line("\x12gp\r")
  end

  def test_ctrl_r_tab_inserts_the_selected_command
    assert_equal "ls", type_line("\x12\t\r")
  end

  def test_ctrl_t_fuzzy_search_inserts_the_selected_path
    FileUtils.mkdir_p(File.join(@tmp, "src"))
    File.write(File.join(@tmp, "src", "unique-file.txt"), "")

    assert_equal "cat src/unique-file.txt", type_line("cat \x14uniq\r\r")
  end
end
