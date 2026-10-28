# frozen_string_literal: true

require_relative "test_helper"
require "sharky"
require "stringio"
require "timeout"

module ColorEnv
  def setup
    super
    @saved_no_color = ENV["NO_COLOR"]
    @saved_sharky_no_color = ENV["SHARKY_NO_COLOR"]
    ENV.delete("NO_COLOR")
    ENV.delete("SHARKY_NO_COLOR")
  end

  def teardown
    ENV["NO_COLOR"] = @saved_no_color
    ENV["SHARKY_NO_COLOR"] = @saved_sharky_no_color
    super
  end
end

class HighlighterTest < Minitest::Test
  include ColorEnv

  def build(known: ["echo"], **styles)
    Sharky::Highlighter.new(
      theme: Sharky::Theme.new(styles),
      command_exists: ->(name) { known.include?(name) }
    )
  end

  def test_known_command_is_not_error_and_unknown_is_red
    highlighter = Sharky::Highlighter.new(
      theme: Sharky::Theme.new(error: "31"),
      command_exists: ->(name) { name == "echo" }
    )

    assert_includes highlighter.highlight("echo hi"), "\e["
    assert_includes highlighter.highlight("nope"), "\e[31m"
  end

  def test_quoted_text_and_operators_are_highlighted
    highlighter = Sharky::Highlighter.new(
      theme: Sharky::Theme.new(quote: "36", operator: "35"),
      command_exists: ->(_name) { true }
    )

    output = highlighter.highlight(%q{echo "a" | wc})
    assert_includes output, "\e[36m"
    assert_includes output, "\e[35m"
  end

  def test_redirects_and_paths_are_highlighted
    output = build(operator: "35", path: "34").highlight("echo hi > /tmp/out")

    assert_includes output, "\e[35m>"
    assert_includes output, "\e[34m/tmp/out"
  end

  def test_comments_are_highlighted
    output = build(comment: "90").highlight("echo hi # note")

    assert_includes output, "\e[90m# note"
  end

  def test_hash_inside_a_word_is_not_a_comment
    output = build(comment: "90").highlight("echo a#b")

    refute_includes output, "\e[90m"
  end

  def test_incomplete_quote_marks_only_the_unclosed_text_as_error
    output = build(error: "31").highlight(%q{echo "hi})

    assert_includes output, "\e[31m\"hi"
    refute_includes output, "\e[31mecho"
  end

  def test_bad_redirect_marks_the_redirect_as_error
    output = build(error: "31", operator: "35").highlight("echo hi >")

    assert_includes output, "\e[31m>"
  end

  def test_trailing_pipe_marks_the_operator_as_error
    output = build(error: "31").highlight("echo hi |")

    assert_includes output, "\e[31m|"
  end

  def test_assignment_before_a_command_is_not_an_unknown_command
    output = build(error: "31").highlight("FOO=1 echo hi")

    refute_includes output, "\e[31mFOO"
  end

  def test_command_after_a_pipe_is_still_a_command
    output = build(error: "31", known: %w[echo wc]).highlight("echo hi | nope")

    assert_includes output, "\e[31mnope"
  end

  def test_quoted_command_name_is_not_flagged_as_unknown
    output = build(error: "31", quote: "36").highlight(%q{echo "nope"})

    refute_includes output, "\e[31m"
  end

  def test_operator_inside_quotes_is_not_an_operator
    output = build(operator: "35", quote: "36").highlight(%q{echo "a|b"})

    refute_includes output, "\e[35m"
  end

  def test_file_descriptor_redirect_does_not_consume_the_command
    output = build(path: "34", known: ["echo"]).highlight("2>&1 echo hi")

    refute_includes output, "\e[34mecho"
  end

  def test_invalid_continuation_after_a_rendered_newline_is_an_error
    output = build(error: "31", known: %w[echo ls]).highlight("echo hi#{Sharky::Editor::NEWLINE_GLYPH}| ls")

    assert_includes output, "\e[31m| ls"
  end

  def test_incomplete_operator_before_a_rendered_newline_is_an_error
    output = build(error: "31", operator: "35").highlight("echo hi |#{Sharky::Editor::NEWLINE_GLYPH}")

    assert_includes output, "\e[31m|"
  end
end

class EditorSuggestionTest < Minitest::Test
  include ColorEnv

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

  def build_editor(history_entries, known: ["echo"])
    reader, writer = IO.pipe
    output = StringIO.new
    terminal = FakeTerminal.new(reader, output)
    history = Sharky::History.new(path: File.join(@tmp, "history"))
    history_entries.each { |entry| history.add(entry) }
    theme = Sharky::Theme.new(suggestion: "38;5;244", error: "31")
    highlighter = Sharky::Highlighter.new(
      theme: theme,
      command_exists: ->(name) { known.include?(name) }
    )
    editor = Sharky::Editor.new(
      terminal: terminal,
      history: history,
      prompt: "> ",
      theme: theme,
      highlighter: highlighter,
      output: output
    )
    [editor, reader, writer, output]
  end

  def type_line(history_entries, keys, known: ["echo"])
    editor, reader, writer, output = build_editor(history_entries, known: known)
    thread = Thread.new { editor.readline }
    writer.write(keys)
    writer.close
    result = Timeout.timeout(5) { thread.value }
    [result, output.string]
  ensure
    thread&.kill if thread&.alive?
    writer&.close unless writer&.closed?
    reader&.close unless reader&.closed?
  end

  def test_history_suggestion_is_rendered_dim_after_the_buffer
    _result, output = type_line(["echo hello"], "echo h\r")

    assert_includes output, "\e[38;5;244mello"
  end

  def test_right_accepts_the_full_suggestion_at_the_end
    result, = type_line(["echo hello"], "echo h\e[C\r")

    assert_equal "echo hello", result
  end

  def test_ctrl_f_accepts_the_full_suggestion_at_the_end
    result, = type_line(["echo hello"], "echo h\x06\r")

    assert_equal "echo hello", result
  end

  def test_alt_right_accepts_one_word_of_the_suggestion
    result, = type_line(["echo hello world"], "echo h\e[1;3C\r")

    assert_equal "echo hello", result
  end

  def test_right_moves_the_cursor_when_it_is_not_at_the_end
    result, = type_line(["echo hello"], "echo h\e[D\e[C\r")

    assert_equal "echo h", result
  end

  def test_empty_buffer_has_no_suggestion
    result, output = type_line(["echo hello"], "\e[C\r")

    assert_equal "", result
    refute_includes output, "\e[38;5;244m"
  end

  def test_editor_renders_highlighted_buffer
    _result, output = type_line([], "nope\r")

    assert_includes output, "\e[31mnope"
  end
end
