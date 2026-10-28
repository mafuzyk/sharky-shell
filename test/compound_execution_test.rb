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
    @stdout.string = ""
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

  def test_default_shell_run_stays_legacy
    assert_equal 2, @shell.run("if true; then echo no; fi")
  end
end
