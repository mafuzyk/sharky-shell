require_relative "test_helper"
require "sharky"
require "stringio"

class SessionTest < Minitest::Test
  def setup
    super
    @previous_session = Sharky.session if Sharky.respond_to?(:session)
    @session = build_session
  end

  def teardown
    Sharky.session = @previous_session if Sharky.respond_to?(:session=)
    super
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

  def test_run_uses_compound_parser
    result = @session.run("if true; then printf yes; fi")

    assert_equal 0, result.status
    assert_equal "yes", result.stdout
    assert_equal "", result.stderr
  end

  def test_run_returns_status_two_for_parse_errors
    result = @session.run("if")

    assert_equal 2, result.status
    assert_equal "", result.stdout
    assert_includes result.stderr, "sharky:"
    assert_includes result.stderr, "unclosed compound command"
  end

  def test_run_restores_shell_streams
    stdout = @session.shell.stdout
    stderr = @session.shell.stderr

    @session.run("printf output")

    assert_same stdout, @session.shell.stdout
    assert_same stderr, @session.shell.stderr
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
    other = build_session
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)

    @session.run("export SHARKY_SESSION_ONLY=one")
    @session.run("cd #{target}")

    assert_nil other.env["SHARKY_SESSION_ONLY"]
    assert_equal @tmp, other.cwd
  end

  def test_facade_delegates_runtime_config_to_current_session
    other = build_session
    Sharky.session = @session

    assert_same @session, Sharky.session
    assert_same @session.runtime, Sharky.runtime

    Sharky.alias "facade_alias", "printf facade"
    Sharky.prompt = ->(_context) { "FACADE> " }
    Sharky.theme = { error: "31" }
    Sharky.command("facade_command") { 0 }

    assert_equal "printf facade", @session.runtime.aliases["facade_alias"]
    assert_equal "FACADE> ", @session.runtime.prompt.call(nil)
    assert_equal "31", @session.runtime.theme[:error]
    assert @session.runtime.commands.key?("facade_command")
    assert_nil other.runtime.aliases["facade_alias"]
    assert_nil other.runtime.prompt
    assert_empty other.runtime.theme
    refute other.runtime.commands.key?("facade_command")
  end

  def test_facade_uses_compound_parser
    Sharky.session = @session

    assert_equal 0, Sharky.sh("if true; then printf compound; fi")
    assert_equal "compound", @session.shell.stdout.string
    assert_equal "captured", Sharky.capture("if true; then printf captured; fi")
  end

  def test_facade_runs_commands_in_current_session_shell
    Sharky.session = @session

    assert_equal 0, Sharky.sh("printf facade")
    assert_equal "facade", @session.shell.stdout.string
    assert_equal 0, Sharky.last_status
    assert_equal "cap", Sharky.capture("printf cap")
    assert_equal "facade", @session.shell.stdout.string
  end

  def test_facade_exposes_current_session_state
    Sharky.session = @session
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)

    Sharky.sh("cd #{target}; export SHARKY_FACADE_STATE=present")

    assert_equal target, Sharky.cwd
    assert_equal target, @session.cwd
    assert_equal "present", Sharky.env["SHARKY_FACADE_STATE"]
  end

  def test_facade_switches_between_isolated_sessions
    other = build_session
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)
    Sharky.session = @session
    Sharky.alias "only_current", "printf one"
    Sharky.sh("cd #{target}; export SHARKY_ONLY_CURRENT=one")

    Sharky.session = other

    assert_same other, Sharky.session
    assert_same other.runtime, Sharky.runtime
    assert_nil other.runtime.aliases["only_current"]
    assert_nil Sharky.env["SHARKY_ONLY_CURRENT"]
    assert_equal @tmp, Sharky.cwd
    refute_same @session.runtime, other.runtime
    refute_same @session.shell, other.shell
  end

  def test_runtime_assignment_reactivates_its_session
    Sharky.session = nil
    Sharky.runtime = @session.runtime

    assert_same @session, Sharky.session
    assert_same @session.runtime, Sharky.runtime
  end

  private

  def build_session
    Sharky::Session.new(
      env: { "PATH" => "/usr/bin:/bin", "HOME" => @tmp },
      cwd: @tmp,
      stdin: StringIO.new
    )
  end
end
