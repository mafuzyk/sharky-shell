require_relative "test_helper"
require "sharky"
require "stringio"

class SessionTest < Minitest::Test
  def setup
    super
    @session = Sharky::Session.new(
      env: { "PATH" => "/usr/bin:/bin", "HOME" => @tmp },
      cwd: @tmp,
      stdin: StringIO.new
    )
  end

  def test_run_returns_structured_success_result
    result = @session.run("printf hello")

    assert_instance_of Sharky::Result, result
    assert_equal 0, result.status
    assert_equal "hello", result.stdout
    assert_equal "", result.stderr
    assert_equal @tmp, result.cwd
    assert_kind_of Integer, result.duration_ms
    assert_operator result.duration_ms, :>=, 0
    assert_equal 1, result.schema_version
    assert_equal({}, result.env_delta)
    assert_predicate result, :frozen?
  end

  def test_run_returns_nonzero_status
    result = @session.run("false")

    assert_equal 1, result.status
    assert_equal "", result.stdout
    assert_equal "", result.stderr
  end

  def test_run_captures_stderr
    result = @session.run("printf oops >&2")

    assert_equal 0, result.status
    assert_equal "", result.stdout
    assert_equal "oops", result.stderr
  end

  def test_run_persists_cwd_and_reports_it
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)

    result = @session.run("cd #{target}")

    assert_equal 0, result.status
    assert_equal target, result.cwd
    assert_equal target, @session.cwd
    assert_equal "#{target}\n", @session.run("pwd").stdout
  end

  def test_run_reports_env_delta_and_persists_environment
    result = @session.run("export SHARKY_SESSION_TEST=present")

    assert_equal "present", result.env_delta["SHARKY_SESSION_TEST"]
    assert_equal "present", @session.env["SHARKY_SESSION_TEST"]

    result = @session.run("unset SHARKY_SESSION_TEST")

    assert result.env_delta.key?("SHARKY_SESSION_TEST")
    assert_nil result.env_delta["SHARKY_SESSION_TEST"]
    assert_nil @session.env["SHARKY_SESSION_TEST"]
  end

  def test_run_returns_status_two_for_parse_errors
    result = @session.run("if")

    assert_equal 2, result.status
    assert_equal "", result.stdout
    assert_includes result.stderr, "sharky:"
    assert_includes result.stderr, "unsupported syntax"
  end

  def test_session_owns_its_engine_and_works_without_global_runtime
    assert_instance_of Sharky::Shell, @session.shell
    assert_instance_of Sharky::Runtime, @session.runtime
    assert_same @session.shell, @session.runtime.shell

    previous = Sharky.runtime
    Sharky.runtime = nil
    result = @session.run("printf isolated")
    assert_equal "isolated", result.stdout
  ensure
    Sharky.runtime = previous
  end

  def test_sessions_do_not_share_env_or_cwd
    other = Sharky::Session.new(
      env: { "PATH" => "/usr/bin:/bin", "HOME" => @tmp },
      cwd: @tmp,
      stdin: StringIO.new
    )
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)

    @session.run("export SHARKY_SESSION_ONLY=one")
    @session.run("cd #{target}")

    assert_nil other.env["SHARKY_SESSION_ONLY"]
    assert_equal @tmp, other.cwd
  end
end
