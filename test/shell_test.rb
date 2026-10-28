require_relative "test_helper"
require "sharky"
require "stringio"

class ShellTest < Minitest::Test
  def setup
    super
    @shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new, stdin: StringIO.new)
  end

  def test_runs_external_commands_and_status
    assert_equal 0, @shell.run("true")
    assert_equal 1, @shell.run("false")
    assert_equal 1, @shell.last_status
  end

  def test_cd_persists_and_pwd_prints
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)
    assert_equal 0, @shell.run("cd #{target}")
    assert_equal target, @shell.cwd
    assert_equal 0, @shell.run("pwd")
    assert_equal "#{target}\n", @shell.stdout.string
  end

  def test_external_commands_run_in_shell_cwd
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)
    assert_equal 0, @shell.run("cd #{target}")
    assert_equal 0, @shell.run("/usr/bin/pwd")
    assert_equal "#{target}\n", @shell.stdout.string
  end

  def test_export_and_unset
    assert_equal 0, @shell.run("export A=1")
    assert_equal "1", @shell.env["A"]
    assert_equal 0, @shell.run("unset A")
    assert_nil @shell.env["A"]
  end

  def test_assignment_only_persists
    assert_equal 0, @shell.run("B=2")
    assert_equal "2", @shell.env["B"]
  end

  def test_unknown_command_is_127
    assert_equal 127, @shell.run("definitely-not-a-command")
    assert_includes @shell.stderr.string, "command not found"
  end

  def test_exit_sets_flag_and_status
    assert_equal 7, @shell.run("exit 7")
    assert @shell.exit_requested?
    assert_equal 7, @shell.last_status
  end

  def test_exit_stops_following_commands
    assert_equal 0, @shell.run("exit 0; printf no")
    assert_equal "", @shell.stdout.string
  end

  def test_parameter_error_is_reported_without_crashing
    assert_equal 1, @shell.run("echo ${MISSING:?oops}")
    assert_equal "", @shell.stdout.string
    assert_match(/MISSING: oops/, @shell.stderr.string)
  end

  def test_arithmetic_error_is_reported_without_crashing
    assert_equal 1, @shell.run("echo $((1 / 0))")
    assert_match(/division by zero/, @shell.stderr.string)
  end

  def test_set_positional_parameters
    assert_equal 0, @shell.run("set -- a b")
    assert_equal %w[a b], @shell.positional
    assert_equal 0, @shell.run(%q{printf '%s|%s' "$1" "$#"})
    assert_equal "a|2", @shell.stdout.string
  end

  def test_set_e_stops_after_failure
    assert_equal 1, @shell.run("set -e; false; printf no")
    assert_equal "", @shell.stdout.string
    assert @shell.exit_requested?
  end

  def test_set_plus_e_continues
    assert_equal 0, @shell.run("set +e; false; printf yes")
    assert_equal "yes", @shell.stdout.string
  end

  def test_read_assigns_multiple_variables
    @shell.stdin = StringIO.new("hello world rest\n")
    assert_equal 0, @shell.run("read A B")
    assert_equal "hello", @shell.env["A"]
    assert_equal "world rest", @shell.env["B"]
  end

  def test_read_eof_and_raw
    @shell.stdin = StringIO.new("")
    assert_equal 1, @shell.run("read -r A")
    @shell.stdin = StringIO.new("literal\\n\n")
    assert_equal 0, @shell.run("read -r A")
    assert_equal "literal\\n", @shell.env["A"]
  end

  def test_set_prints_options_and_rejects_unknown
    assert_equal 0, @shell.run("set -e")
    assert_equal 0, @shell.run("set")
    assert_includes @shell.stdout.string, "errexit"
    assert_equal 1, @shell.run("set -Z")
    assert_match(/invalid option/, @shell.stderr.string)
  end
end
