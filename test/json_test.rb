require_relative "test_helper"
require "json"
require "shellwords"

class JsonTest < Minitest::Test
  def test_c_success_returns_one_json_envelope
    out, err, status = run_sharky("--json", "-c", "printf hello", env: json_env)

    assert_equal 0, status.exitstatus
    assert_equal "", err
    payload = parse_only_object(out)
    assert_equal %w[schema_version status stdout stderr duration_ms cwd env_delta], payload.keys
    assert_equal 1, payload["schema_version"]
    assert_equal 0, payload["status"]
    assert_equal "hello", payload["stdout"]
    assert_equal "", payload["stderr"]
    assert_kind_of Integer, payload["duration_ms"]
    assert_operator payload["duration_ms"], :>=, 0
    assert_equal Dir.pwd, payload["cwd"]
    assert_equal({}, payload["env_delta"])
  end

  def test_c_failure_returns_json_and_the_command_status
    out, err, status = run_sharky("--json", "-c", "false", env: json_env)

    assert_equal 1, status.exitstatus
    assert_equal "", err
    payload = parse_only_object(out)
    assert_equal 1, payload["status"]
    assert_equal "", payload["stdout"]
    assert_equal "", payload["stderr"]
  end

  def test_c_keeps_command_stderr_out_of_stdout
    out, err, status = run_sharky("--json", "-c", "printf oops >&2", env: json_env)

    assert_equal 0, status.exitstatus
    assert_equal "", err
    payload = parse_only_object(out)
    assert_equal 0, payload["status"]
    assert_equal "", payload["stdout"]
    assert_equal "oops", payload["stderr"]
  end

  def test_c_reports_the_resulting_cwd
    target = File.join(@tmp, "sub")
    Dir.mkdir(target)

    out, _err, status = run_sharky("--json", "-c", "cd #{Shellwords.escape(target)}", env: json_env)

    assert_equal 0, status.exitstatus
    assert_equal target, parse_only_object(out)["cwd"]
  end

  def test_c_reports_the_environment_delta
    out, _err, status = run_sharky("--json", "-c", "export SHARKY_JSON_TEST=present", env: json_env)

    assert_equal 0, status.exitstatus
    assert_equal "present", parse_only_object(out)["env_delta"]["SHARKY_JSON_TEST"]
  end

  def test_c_sanitizes_non_utf8_output_and_preserves_status
    out, err, status = run_sharky(
      "--json", "-c", "/usr/bin/printf '\\377'; /usr/bin/printf '\\376' >&2", env: json_env
    )

    assert_equal 0, status.exitstatus
    assert_equal "", err
    payload = parse_only_object(out)
    assert_equal 0, payload["status"]
    assert_equal "\uFFFD", payload["stdout"]
    assert_equal "\uFFFD", payload["stderr"]
  end

  def test_c_keeps_stdout_valid_json_for_an_unknown_command
    out, err, status = run_sharky("--json", "-c", "definitely-not-a-command", env: json_env)

    assert_equal 127, status.exitstatus
    assert_equal "", err
    payload = parse_only_object(out)
    assert_equal 127, payload["status"]
    assert_equal "", payload["stdout"]
    assert_includes payload["stderr"], "command not found"
  end

  def test_c_keeps_stdout_valid_json_for_a_parse_error
    out, _err, status = run_sharky("--json", "-c", "if", env: json_env)

    assert_equal 2, status.exitstatus
    payload = parse_only_object(out)
    assert_equal 2, payload["status"]
    assert_includes payload["stderr"], "unclosed compound command"
  end

  def test_c_keeps_config_output_out_of_stdout
    config_dir = File.join(@tmp, "config")
    Dir.mkdir(config_dir)
    File.write(File.join(config_dir, "sharky.rb"), "puts 'config noise'\n")

    out, err, status = run_sharky("--json", "-c", "printf ok", env: json_env("SHARKY_CONFIG" => config_dir))

    assert_equal 0, status.exitstatus
    payload = parse_only_object(out)
    assert_equal "ok", payload["stdout"]
    assert_includes err, "config noise"
  end

  def test_state_omits_the_environment_by_default
    out, err, status = Dir.chdir(@tmp) do
      run_sharky("state", "--json", env: json_env("SHARKY_JSON_SECRET" => "fake-secret"))
    end

    assert_equal 0, status.exitstatus
    assert_equal "", err
    payload = parse_only_object(out)
    assert_equal %w[schema_version cwd last_status pid], payload.keys
    refute_includes out, "fake-secret"
    assert_equal 1, payload["schema_version"]
    assert_equal @tmp, payload["cwd"]
    assert_equal 0, payload["last_status"]
    assert_kind_of Integer, payload["pid"]
    assert_operator payload["pid"], :>, 0
  end

  def test_state_env_flag_includes_the_environment
    out, err, status = Dir.chdir(@tmp) do
      run_sharky("state", "--json", "--env", env: json_env("SHARKY_JSON_SECRET" => "fake-secret"))
    end

    assert_equal 0, status.exitstatus
    assert_equal "", err
    payload = parse_only_object(out)
    assert_equal %w[schema_version cwd env last_status pid], payload.keys
    assert_equal 1, payload["schema_version"]
    assert_equal @tmp, payload["cwd"]
    assert_equal "fake-secret", payload["env"]["SHARKY_JSON_SECRET"]
    assert_equal 0, payload["last_status"]
    assert_kind_of Integer, payload["pid"]
    assert_operator payload["pid"], :>, 0
  end

  def test_env_flag_requires_state
    out, err, status = run_sharky("--env", env: json_env)

    assert_equal 2, status.exitstatus
    assert_equal "", out
    assert_includes err, "--env is only supported with state"
  end

  def test_state_rejects_a_missing_json_flag
    out, err, status = run_sharky("state", env: json_env)

    assert_equal 2, status.exitstatus
    assert_equal "", out
    assert_includes err, "state requires --json"
  end

  private

  def json_env(extra = {})
    {
      "XDG_CONFIG_HOME" => File.join(@tmp, "xdg"),
      "SHARKY_CONFIG" => nil
    }.merge(extra)
  end

  def parse_only_object(out)
    assert_equal 1, out.lines.length, "stdout must contain exactly one JSON line"
    JSON.parse(out)
  end
end
