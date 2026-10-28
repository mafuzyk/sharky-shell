require_relative "test_helper"
require "sharky"
require "stringio"

class AcceptanceTest < Minitest::Test
  def setup
    super
    @previous_runtime = Sharky.runtime
  end

  def teardown
    Sharky.runtime = @previous_runtime
    super
  end

  def test_pipeline_and_status
    out, _err, status = run_sharky("-c", "printf 'a b\\n' | wc -w")
    assert_equal 0, status.exitstatus
    assert_equal "2\n", out
  end

  def test_state_and_logic
    out, _err, status = run_sharky("-c", "cd /tmp && pwd; export A=1; printenv A; false && echo no; true || echo no; echo yes")
    assert_equal 0, status.exitstatus
    assert_equal "/tmp\n1\nyes\n", out
  end

  def test_for_loop_executes_through_cli
    out, err, status = run_sharky("-c", "for x in a b; do printf '%s' \"$x\"; done")
    assert_equal 0, status.exitstatus
    assert_equal "ab", out
    assert_empty err
  end

  def test_unsupported_syntax_points_to_bash_escape
    _out, err, status = run_sharky("-c", "coproc true")
    assert_equal 2, status.exitstatus
    assert_includes err, "unsupported syntax: 'coproc'. Use bash -c '...' or a Ruby function."
  end

  def test_unsupported_syntax_caret_points_to_command_start
    _out, err, status = run_sharky("-c", "coproc true")
    assert_equal 2, status.exitstatus
    assert_match(/sharky: coproc true\nsharky: \^\n/, err)
  end

  def test_unsupported_operators_point_to_bash_escape
    _out, err, status = run_sharky("-c", "echo @(a|b)")
    assert_equal 2, status.exitstatus
    assert_includes err, "unsupported syntax"
  end

  def test_double_bracket_test_executes
    _out, err, status = run_sharky("-c", "[[ -n x ]]")
    assert_equal 0, status.exitstatus
    assert_empty err
  end

  def test_parse_error_shows_source_and_caret
    _out, err, status = run_sharky("-c", "echo \"unclosed")
    assert_equal 2, status.exitstatus
    assert_includes err, "echo \"unclosed"
    assert_includes err, "^"
  end

  def test_arithmetic_expansion_is_supported
    out, err, status = run_sharky("-c", "echo $((1 + 2))")
    assert_equal 0, status.exitstatus
    assert_equal "3\n", out
    assert_empty err
  end

  def test_extglob_is_rejected_without_executing
    out, err, status = run_sharky("-c", "echo @(a|b)")
    assert_equal 2, status.exitstatus
    assert_empty out
    assert_includes err, "unsupported syntax"
  end

  def test_missing_separator_executes_nothing
    out, err, status = run_sharky("-c", "echo hi(echo side)")
    assert_equal 2, status.exitstatus
    assert_empty out
    assert_includes err, "unexpected"
  end

  def test_array_assignment_executes_nothing
    out, _err, status = run_sharky("-c", "x=(1 2)")
    assert_equal 2, status.exitstatus
    assert_empty out
  end

  def test_unknown_command
    _out, err, status = run_sharky("-c", "definitely-not-a-command")
    assert_equal 127, status.exitstatus
    assert_includes err, "command not found"
  end

  def test_enoeexec
    script = File.join(@tmp, "plain")
    File.write(script, "printf 'should not run\\n'\n")
    File.chmod(0o755, script)

    out, err, status = run_sharky("-c", script)
    assert_equal 126, status.exitstatus
    assert_empty out
    assert_includes err, "bash"
  end

  def test_permission_denied
    script = File.join(@tmp, "private")
    File.write(script, "printf 'should not run\\n'\n")
    File.chmod(0o644, script)

    _out, err, status = run_sharky("-c", script)
    assert_equal 126, status.exitstatus
    assert_includes err, "permission denied"
  end

  def test_enoeexec_binary_has_hint
    binary = File.join(@tmp, "wrong-arch")
    File.binwrite(binary, "\x7FELF\x02\x01\x01")
    File.chmod(0o755, binary)

    _out, err, status = run_sharky("-c", binary)
    assert_equal 126, status.exitstatus
    assert_includes err, "bash"
  end

  def test_theme_file_and_prompt_change_the_next_prompt
    config_dir = File.join(@tmp, "sharky")
    themes = File.join(config_dir, "themes")
    FileUtils.mkdir_p(themes)
    File.write(File.join(themes, "ocean.rb"), "Sharky.theme = { error: '31' }\n")
    File.write(File.join(config_dir, "sharky.rb"), <<~RUBY)
      Sharky.theme :ocean
      Sharky.prompt = ->(_ctx) { "custom> " }
    RUBY

    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    assert_empty Sharky::Config.new(dir: config_dir).load_into(runtime)

    theme = Sharky::Theme.new(runtime.theme)
    context = Sharky::Prompt::Context.new(cwd: @tmp, home: @tmp, status: 1, duration_ms: 0, jobs_count: 0)
    assert_equal "custom> ", Sharky::Prompt.render(context, theme: theme)
    assert_equal "31", theme.styles[:error]
  end

  def test_reload_reevaluates_init_and_clears_configuration
    config_dir = File.join(@tmp, "sharky")
    FileUtils.mkdir_p(config_dir)
    init = File.join(config_dir, "sharky.rb")
    File.write(init, "Sharky.alias 'old_name', 'printf old'\n")

    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    assert_empty Sharky::Config.new(dir: config_dir).load_into(runtime)

    stdout = StringIO.new
    stderr = StringIO.new
    shell = Sharky::Shell.new(env: { "PATH" => ENV["PATH"] }, cwd: @tmp, stdout: stdout, stderr: stderr, runtime: runtime)
    assert_equal 0, shell.run("old_name")

    File.write(init, "Sharky.alias 'new_name', 'printf new'\n")
    assert_equal 0, shell.run("reload")
    assert_empty stderr.string

    assert_equal 1, shell.run("type old_name")
    assert_includes stderr.string, "not found"
    assert_equal 0, shell.run("new_name")
    assert_equal "oldnew", stdout.string
  end

  def test_brace_expansion_reaches_the_cli
    out, err, status = run_sharky("-c", "printf '%s\\n' a{b,c}d")
    assert_equal 0, status.exitstatus
    assert_equal "abd\nacd\n", out
    assert_empty err
  end
end
