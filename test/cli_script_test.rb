require_relative "test_helper"
require "shellwords"
require "json"

class CLIScriptTest < Minitest::Test
  def script(name, content, mode: 0o644)
    path = File.join(@tmp, name)
    File.write(path, content)
    File.chmod(mode, path)
    path
  end

  def test_sh_script_sets_positionals_and_status
    path = script("args.sh", "printf '%s|%s|%s' \"$0\" \"$1\" \"$#\"\nexit 7\n")
    out, _err, status = run_sharky(path, "a", "b")

    assert_equal 7, status.exitstatus
    assert_equal "#{path}|a|2", out
  end

  def test_sharky_shebang_runs_directly
    path = script("hello.sh", "#!/usr/bin/env sharky\nprintf 'shebang'\n", mode: 0o755)
    env = {
      "HOME" => @tmp,
      "PATH" => "#{File.join(ROOT, 'bin')}:#{ENV['PATH']}"
    }
    out, err, status = Open3.capture3(env, path)

    assert_equal 0, status.exitstatus
    assert_equal "shebang", out
    assert_empty err
  end

  def test_source_persists_env_and_sets_source_args
    sourced = script("lib.sh", "SOURCED=yes\nprintf 's:%s' \"$1\"\n")
    command = "source #{Shellwords.escape(sourced)} arg; printf '|%s|%s' \"$SOURCED\" \"$1\""
    out, err, status = run_sharky("-c", command)

    assert_equal 0, status.exitstatus
    assert_equal "s:arg|yes|", out
    assert_empty err
  end

  def test_missing_source_is_status_one
    out, err, status = run_sharky("-c", "source /definitely/missing.sh")

    assert_equal 1, status.exitstatus
    assert_empty out
    assert_match(/source: .*not found/, err)
  end

  def test_source_persists_cwd
    sub = File.join(@tmp, "sub")
    Dir.mkdir(sub)
    sourced = script("cwd.sh", "cd ..\n")
    out, err, status = run_sharky("-c", "cd #{Shellwords.escape(sub)}; source #{Shellwords.escape(sourced)}; pwd")

    assert_equal 0, status.exitstatus
    assert_equal "#{@tmp}\n", out
    assert_empty err
  end

  def test_ruby_script_still_runs
    path = script("legacy.rb", "puts 'ruby'\n")
    out, err, status = run_sharky(path)

    assert_equal 0, status.exitstatus
    assert_equal "ruby\n", out
    assert_empty err
  end

  def test_set_e_stops_a_cli_script
    out, _err, status = run_sharky("-c", "set -e; false; echo no")

    assert_equal 1, status.exitstatus
    assert_equal "", out
  end

  def test_json_captures_stderr_from_command_substitution
    command = 'printf \'%s\' "$(printf diagnostic >&2; printf result)"'
    out, err, status = run_sharky("--no-config", "--json", "-c", command)

    assert_equal 0, status.exitstatus
    assert_empty err
    payload = JSON.parse(out)
    assert_equal "result", payload.fetch("stdout")
    assert_equal "diagnostic", payload.fetch("stderr")
  end

  def test_incomplete_pattern_comparison_reports_syntax_error_without_crashing
    out, err, status = run_sharky("--no-config", "-c", "[[ value == ]]")

    assert_equal 2, status.exitstatus
    assert_empty out
    assert_match(/syntax error/, err)
    refute_match(/IndexError|executor\.rb/, err)
  end

  def test_read_consumes_cli_stdin
    out, err, status = run_sharky("-c", "read x; printf '%s' \"$x\"", stdin: "hello\n")

    assert_equal 0, status.exitstatus
    assert_equal "hello", out
    assert_empty err
  end
end
