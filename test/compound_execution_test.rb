require_relative "test_helper"
require "sharky"
require "stringio"

class CompoundExecutionTest < Minitest::Test
  def setup
    super
    @stdout = StringIO.new
    @stderr = StringIO.new
    @shell = Sharky::Shell.new(
      env: { "PATH" => "/usr/bin:/bin" },
      cwd: @tmp,
      stdin: StringIO.new,
      stdout: @stdout,
      stderr: @stderr
    )
  end

  def run_compound(source)
    @shell.run(source, compound: true)
  end

  def test_if_true_branch_and_status
    assert_equal 0, run_compound("if true; then echo yes; fi")
    assert_equal "yes\n", @stdout.string
  end

  def test_if_false_branch_else_and_elif
    run_compound("if false; then echo no; elif true; then echo elif; else echo else; fi")
    assert_equal "elif\n", @stdout.string
  end

  def test_nested_if
    run_compound("if true; then if false; then echo no; else echo nested; fi; fi")
    assert_equal "nested\n", @stdout.string
  end

  def test_multiple_assignments_without_a_command_persist
    assert_equal 0, run_compound(%q{A=1 B=2; printf '%s|%s\n' "$A" "$B"})
    assert_equal "1|2\n", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_for_loop
    run_compound("for x in a b; do printf '%s' \"$x\"; done")
    assert_equal "ab", @stdout.string
  end

  def test_while_loop
    run_compound("i=0; while [ \"$i\" -lt 3 ]; do printf '%s' \"$i\"; i=$((i + 1)); done")
    assert_equal "012", @stdout.string
  end

  def test_until_loop
    run_compound("i=0; until [ \"$i\" -ge 2 ]; do printf '%s' \"$i\"; i=$((i + 1)); done")
    assert_equal "01", @stdout.string
  end

  def test_group_and_not
    run_compound("{ echo group; }")
    assert_equal "group\n", @stdout.string
    @stdout.truncate(0)
    @stdout.rewind
    assert_equal 0, run_compound("! false")
    assert_equal 1, run_compound("! true")
  end

  def test_arithmetic_command_status
    assert_equal 0, run_compound("((1 + 2))")
    assert_equal 1, run_compound("((0))")
    assert_equal 1, run_compound("((1 / 0))")
    assert_match(/division by zero/, @stderr.string)
  end

  def test_simple_function_definition_and_call
    assert_equal 0, run_compound("f() { echo hi; }; f")
    assert_equal "hi\n", @stdout.string
  end

  def test_double_bracket_tests
    assert_equal 0, run_compound("[[ -n x ]]")
    assert_equal 1, run_compound("[[ -z x ]]")
    assert_equal 0, run_compound("[[ a == a ]]")
    assert_equal 1, run_compound("[[ a != a ]]")
    assert_equal 1, run_compound("[[ -n x && -z y ]]")
  end

  def test_double_bracket_rejects_unknown_operators
    assert_equal 2, run_compound("[[ x BOGUS y ]]")
    assert_match(/syntax error/, @stderr.string)
  end

  def test_double_bracket_supports_symlink_and_lexical_comparisons
    assert_equal 1, run_compound("[[ -L definitely-missing ]]")
    assert_equal 0, run_compound("[[ x = x ]]")
    assert_equal 1, run_compound("[[ x = y ]]")
    assert_equal 0, run_compound("[[ a < b ]]")
    assert_equal 1, run_compound("[[ z < y ]]")
  end

  def test_case_matches_first_arm_and_glob
    assert_equal 0, run_compound("case abc in a*) echo yes;; *) echo no;; esac")
    assert_equal "yes\n", @stdout.string
    @stdout.truncate(0)
    @stdout.rewind
    assert_equal 0, run_compound("case zzz in a*) echo yes;; esac")
    assert_equal "", @stdout.string
  end

  def test_here_document_feeds_stdin
    run_compound("cat <<EOF\nhello\nEOF\n")
    assert_equal "hello\n", @stdout.string
  end

  def test_here_document_body_is_not_lexed_as_shell_syntax
    assert_equal 0, run_compound("cat <<'EOF'\n'\nEOF\nprintf after")
    assert_equal "'\nafter", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_here_document_quoted_delimiter_does_not_expand
    @shell.env["X"] = "world"
    run_compound("cat <<'EOF'\nhello $X\nEOF\n")
    assert_equal "hello $X\n", @stdout.string
    @stdout.truncate(0)
    @stdout.rewind
    run_compound("cat <<EOF\nhello $X\nEOF\n")
    assert_equal "hello world\n", @stdout.string
  end

  def test_here_document_unquoted_variable_delimiter_is_literal
    @shell.env["X"] = "EOF"

    status = run_compound(<<~SH)
      cat <<$X
      hi
      $X
    SH

    assert_equal 0, status
    assert_equal "hi\n", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_here_string_appends_newline
    run_compound("cat <<< hello")
    assert_equal "hello\n", @stdout.string
  end

  def reset_stdout
    @stdout.truncate(0)
    @stdout.rewind
  end

  def test_break_and_continue
    run_compound("for x in a b c; do [ \"$x\" = b ] && break; printf '%s' \"$x\"; done")
    assert_equal "a", @stdout.string
    reset_stdout

    run_compound("for x in a b c; do [ \"$x\" = b ] && continue; printf '%s' \"$x\"; done")
    assert_equal "ac", @stdout.string
  end

  def test_break_two_levels
    run_compound("for x in a b; do for y in 1 2; do break 2; done; echo no; done; echo done")
    assert_equal "done\n", @stdout.string
  end

  def test_return_from_function
    assert_equal 7, run_compound("f() { echo before; return 7; echo after; }; f")
    assert_equal "before\n", @stdout.string
  end

  def test_control_flow_misuse_is_status_one
    assert_equal 1, run_compound("break")
    assert_match(/break: only meaningful in a loop/, @stderr.string)
    assert_equal 1, run_compound("return 3")
    assert_match(/return: only meaningful in a function/, @stderr.string)
  end

  def test_function_arguments_and_shift
    assert_equal 0, run_compound("f() { printf '%s|%s' \"$1\" \"$2\"; }; f a b")
    assert_equal "a|b", @stdout.string
    reset_stdout

    run_compound("f() { printf '%s' \"$#\"; }; f a b")
    assert_equal "2", @stdout.string
    reset_stdout

    run_compound("f() { shift; printf '%s' \"$1\"; }; f a b")
    assert_equal "b", @stdout.string
  end

  def test_shift_past_end_status
    assert_equal 1, run_compound("f() { shift 3; }; f a")
  end

  def test_nested_function_restores_positional
    run_compound("f() { printf '%s' \"$1\"; g x; printf '%s' \"$1\"; }; g() { printf '%s' \"$1\"; }; f outer")
    assert_equal "outerxouter", @stdout.string
  end

  def test_subshell_redirect_covers_every_command
    assert_equal 0, run_compound("(printf a; printf b) >/dev/null")
    assert_equal "", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_compound_trailing_redirections
    file = File.join(@tmp, "compound-out")
    assert_equal 0, run_compound("if true; then echo redirected; fi > #{file}")
    assert_equal "redirected\n", File.read(file)

    assert_equal 0, run_compound("for x in a b; do echo \"$x\"; done >> #{file}")
    assert_equal "redirected\na\nb\n", File.read(file)

    assert_equal 0, run_compound("{ echo grouped; } > #{file}")
    assert_equal "grouped\n", File.read(file)
  end

  def test_compound_redirection_preserves_status
    file = File.join(@tmp, "compound-status")
    assert_equal 1, run_compound("{ false; } > #{file}")
    assert_equal "", File.read(file)
  end

  def test_compound_pipeline_and_background
    run_compound("if true; then echo piped; fi | cat")
    assert_includes @stdout.string, "piped"

    reset_stdout
    run_compound("for x in a b; do echo \"$x\"; done | wc -l")
    assert_equal "2\n", @stdout.string

    reset_stdout
    file = File.join(@tmp, "background-compound")
    assert_equal 0, run_compound("if true; then echo background; fi > #{file} & wait")
    assert_equal "background\n", File.read(file)
  end

  def test_command_substitution_inherits_compound_mode
    assert_equal 0, run_compound(%q{printf '%s\n' "$(if true; then printf yes; fi)"})
    assert_equal "yes\n", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_and_or_allows_newlines_after_the_operator
    assert_equal 0, run_compound("true &&\nprintf yes; false ||\nprintf no")
    assert_equal "yesno", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_pipeline_allows_newlines_after_the_operator
    assert_equal 0, run_compound("printf piped |\ncat")
    assert_equal "piped", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_subshell_accepts_multiple_lines
    assert_equal 0, run_compound("(\nprintf a\n\nprintf b\n)")
    assert_equal "ab", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_adjacent_subshell_closers_are_not_arithmetic_delimiters
    assert_equal 0, run_compound("( (printf nested))")
    assert_equal "nested", @stdout.string
    assert_equal 0, run_compound("((1 + ((2))))")
    assert_equal "", @stderr.string
  end

  def test_bang_and_open_brace_can_be_command_arguments
    assert_equal 0, run_compound(%q(printf '%s\n' ! {))
    assert_equal "!\n{\n", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_double_brackets_inside_an_argument_are_not_a_test_command
    assert_equal 0, run_compound(%q{printf '%s\n' [[foo]]})
    assert_equal "[[foo]]\n", @stdout.string
    assert_equal "", @stderr.string
  end

  def test_default_shell_run_stays_legacy
    assert_equal 2, @shell.run("if true; then echo no; fi")
  end
end
