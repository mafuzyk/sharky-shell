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
end
