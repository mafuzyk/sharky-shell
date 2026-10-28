require_relative "test_helper"

class CLICommandTest < Minitest::Test
  def test_c_runs_command_and_returns_status
    out, _err, status = run_sharky("-c", "printf 'a b\\n' | wc -w")
    assert_equal 0, status.exitstatus
    assert_equal "2\n", out
  end

  def test_c_propagates_failure
    _out, err, status = run_sharky("-c", "definitely-not-a-command")
    assert_equal 127, status.exitstatus
    assert_includes err, "command not found"
  end

  def test_no_config_is_accepted
    out, _err, status = run_sharky("--no-config", "-c", "printf ok")
    assert_equal 0, status.exitstatus
    assert_equal "ok", out
  end

  def test_c_passes_stdin
    out, _err, status = run_sharky("-c", "cat", stdin: "from stdin")
    assert_equal 0, status.exitstatus
    assert_equal "from stdin", out
  end

  def test_missing_c_argument_is_an_error
    out, err, status = run_sharky("-c")
    assert_equal 2, status.exitstatus
    assert_equal "", out
    assert_includes err, "-c"
  end

  def test_unknown_option_is_an_error
    out, err, status = run_sharky("--wat")
    assert_equal 2, status.exitstatus
    assert_equal "", out
    assert_includes err, "unknown option"
  end

  def test_i_is_ignored_with_c
    out, _err, status = run_sharky("-i", "-c", "printf ok")
    assert_equal 0, status.exitstatus
    assert_equal "ok", out
  end

  def test_help
    out, _err, status = run_sharky("--help")
    assert_equal 0, status.exitstatus
    assert_includes out, "Usage: sharky"
  end
end
