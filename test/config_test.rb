require_relative "test_helper"
require "sharky"

class ConfigTest < Minitest::Test
  ShellStub = Struct.new(:cwd, :env, :last_status, :jobs)
  JobsStub = Struct.new(:all)
  LayoutStub = Struct.new(:text) do
    def render(_context, theme:)
      theme.style(:directory, text)
    end
  end

  def setup
    super
    @previous_runtime = Sharky.runtime
  end

  def teardown
    Sharky.runtime = @previous_runtime
    super
  end

  def test_loads_sharky_rb_and_registers_configuration
    config_dir = File.join(@tmp, "sharky")
    FileUtils.mkdir_p(config_dir)
    File.write(File.join(config_dir, "sharky.rb"), <<~RUBY)
      Sharky.alias "ll", "printf ll"
      Sharky.abbr "gco", "git checkout"
      Sharky.prompt = ->(ctx) { "P> " }
      Sharky.theme = { error: "31" }
    RUBY

    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    config = Sharky::Config.new(dir: config_dir)
    warnings = config.load_into(runtime)

    assert_empty warnings
    assert_equal "printf ll", runtime.aliases["ll"]
    assert_equal "git checkout", runtime.abbreviations["gco"]
    assert_equal "P> ", runtime.prompt.call(nil)
    assert_equal "31", runtime.theme[:error]
  end

  def test_prompt_preset_installs_builtin_callable
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime

    assert_respond_to Sharky, :prompt_preset
    Sharky.prompt_preset(:simple)

    assert_same Sharky::PromptPresets[:simple], runtime.prompt
    assert_respond_to runtime.prompt, :call
  end

  def test_starship_stores_provider_options
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    command = fake_starship
    config = File.join(@tmp, "starship.toml")

    assert_respond_to Sharky, :starship
    Sharky.starship(config: config, command: command, timeout: 0.5)

    assert_equal({ config: config, command: command, timeout: 0.5 }, runtime.starship)
  end

  def test_starship_defaults_to_the_official_command
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime

    Sharky.starship

    assert_equal({ config: nil, command: "starship", timeout: 0.2 }, runtime.starship)
  end

  def test_starship_prompt_is_used_when_no_explicit_prompt_is_set
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    runtime.starship = { config: nil, command: fake_starship, timeout: 1 }

    with_no_color { assert_equal "STARSHIP> ", build_prompt(runtime) }
  end

  def test_explicit_prompt_wins_over_starship
    marker = File.join(@tmp, "starship-called")
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    runtime.prompt = ->(_context) { "EXPLICIT> " }
    runtime.starship = { config: nil, command: fake_starship(marker: marker), timeout: 1 }

    with_no_color { assert_equal "EXPLICIT> ", build_prompt(runtime) }
    refute File.exist?(marker), "starship should not run when an explicit prompt is set"
  end

  def test_starship_wins_over_theme_layout
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    runtime.theme_layout = LayoutStub.new("LAYOUT> ")
    runtime.starship = { config: nil, command: fake_starship, timeout: 1 }

    with_no_color { assert_equal "STARSHIP> ", build_prompt(runtime) }
  end

  def test_theme_layout_is_used_without_starship
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    runtime.theme_layout = LayoutStub.new("LAYOUT> ")

    with_no_color { assert_equal "LAYOUT> ", build_prompt(runtime) }
  end

  def test_starship_failure_falls_back_to_theme_layout
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    runtime.theme_layout = LayoutStub.new("LAYOUT> ")
    runtime.starship = { config: nil, command: fake_starship(status: 7), timeout: 1 }

    with_no_color { assert_equal "LAYOUT> ", build_prompt(runtime) }
  end

  def test_starship_failure_falls_back_to_default_prompt
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    runtime.starship = { config: nil, command: fake_starship(status: 7), timeout: 1 }

    with_no_color { assert_equal "~ ❯ ", build_prompt(runtime) }
  end

  def test_config_error_is_reported_and_does_not_raise
    config_dir = File.join(@tmp, "sharky")
    FileUtils.mkdir_p(config_dir)
    File.write(File.join(config_dir, "sharky.rb"), "raise 'boom'\n")
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    warnings = Sharky::Config.new(dir: config_dir).load_into(runtime)
    assert_equal 1, warnings.size
    assert_includes warnings.first, "boom"
  end

  def test_sharky_config_env_overrides
    config_dir = File.join(@tmp, "custom")
    FileUtils.mkdir_p(config_dir)
    File.write(File.join(config_dir, "sharky.rb"), "Sharky.alias 'x', 'printf x'\n")
    old = ENV["SHARKY_CONFIG"]
    ENV["SHARKY_CONFIG"] = config_dir
    assert_equal config_dir, Sharky::Config.new.config_dir
  ensure
    ENV["SHARKY_CONFIG"] = old
  end

  def test_alias_builtin_and_type
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new)
    shell.runtime = runtime
    assert_equal 0, shell.run("alias ll 'printf ll'")
    assert_equal "printf ll", runtime.aliases["ll"]
    assert_equal 0, shell.run("type ll")
    assert_includes shell.stdout.string, "alias"
  end

  def test_alias_rejects_control_operators
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new)
    shell.runtime = runtime
    assert_equal 1, shell.run("alias bad 'echo x | cat'")
    assert_includes shell.stderr.string, "control operator"
  end

  def test_alias_expands_and_appends_arguments
    runtime = Sharky::Runtime.new
    shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new, runtime: runtime)
    runtime.aliases["show"] = "printf '%s\\n'"
    assert_equal 0, shell.run("show one two")
    assert_equal "one\ntwo\n", shell.stdout.string
  end

  def test_unalias_and_abbreviation_builtins
    runtime = Sharky::Runtime.new
    shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new, runtime: runtime)
    assert_equal 0, shell.run("abbr gco 'git checkout'")
    assert_equal "git checkout", runtime.abbreviations["gco"]
    assert_equal 0, shell.run("abbr")
    assert_includes shell.stdout.string, "gco"
    assert_equal 0, shell.run("unabbr gco")
    assert_empty runtime.abbreviations

    runtime.aliases["ll"] = "printf ll"
    assert_equal 0, shell.run("unalias ll")
    assert_empty runtime.aliases
    assert_equal 1, shell.run("unalias missing")
  end

  def test_type_reports_function_builtin_and_executable
    runtime = Sharky::Runtime.new
    runtime.commands["hello"] = ->(_argv, _stdin) { 0 }
    shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new, runtime: runtime)
    assert_equal 0, shell.run("type hello cd sh")
    assert_includes shell.stdout.string, "hello: function"
    assert_includes shell.stdout.string, "cd: shell builtin"
    assert_includes shell.stdout.string, "sh: /"
  end

  def test_api_setters_require_a_runtime
    old = Sharky.runtime
    Sharky.runtime = nil
    error = assert_raises(RuntimeError) { Sharky.alias "x", "printf x" }
    assert_includes error.message, "runtime"
  ensure
    Sharky.runtime = old
  end

  def test_cli_loads_config_and_honors_no_config
    config_dir = File.join(@tmp, "sharky")
    FileUtils.mkdir_p(config_dir)
    File.write(File.join(config_dir, "sharky.rb"), "Sharky.alias '__sharky_config_greet', \"printf from-config\"\n")
    out, _err, status = run_sharky("-c", "__sharky_config_greet", env: { "SHARKY_CONFIG" => config_dir })
    assert_equal 0, status.exitstatus
    assert_equal "from-config", out

    out, err, status = run_sharky("--no-config", "-c", "__sharky_config_greet", env: { "SHARKY_CONFIG" => config_dir })
    assert_equal 127, status.exitstatus
    assert_empty out
    assert_includes err, "command not found"
  end

  def test_xdg_and_default_paths
    old = ENV.to_h.slice("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "SHARKY_CONFIG")
    ENV["HOME"] = @tmp
    ENV["XDG_CONFIG_HOME"] = File.join(@tmp, "config")
    ENV["XDG_DATA_HOME"] = File.join(@tmp, "data")
    ENV["XDG_CACHE_HOME"] = File.join(@tmp, "cache")
    ENV.delete("SHARKY_CONFIG")

    config = Sharky::Config.new
    assert_equal File.join(@tmp, "config", "sharky"), config.config_dir
    assert_equal File.join(@tmp, "data", "sharky"), config.data_dir
    assert_equal File.join(@tmp, "cache", "sharky"), config.cache_dir

    ENV.delete("XDG_CONFIG_HOME")
    ENV.delete("XDG_DATA_HOME")
    ENV.delete("XDG_CACHE_HOME")
    config = Sharky::Config.new
    assert_equal File.join(@tmp, ".config", "sharky"), config.config_dir
    assert_equal File.join(@tmp, ".local", "share", "sharky"), config.data_dir
    assert_equal File.join(@tmp, ".cache", "sharky"), config.cache_dir
  ensure
    %w[HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME SHARKY_CONFIG].each do |name|
      if old && old.key?(name)
        ENV[name] = old[name]
      else
        ENV.delete(name)
      end
    end
  end

  def test_ensure_layout_creates_empty_config_once
    dir = File.join(@tmp, "newconfig")
    config = Sharky::Config.new(dir: dir)

    assert config.ensure_layout!
    assert File.directory?(dir)
    assert_equal "", File.read(File.join(dir, "sharky.rb"))
    refute config.ensure_layout!
  end

  def test_ensure_layout_does_not_overwrite_existing_config
    dir = File.join(@tmp, "existingconfig")
    FileUtils.mkdir_p(dir)
    path = File.join(dir, "sharky.rb")
    File.write(path, "# keep me\n")

    refute Sharky::Config.new(dir: dir).ensure_layout!
    assert_equal "# keep me\n", File.read(path)
  end

  def test_legacy_init_rb_still_loads_with_warning
    dir = File.join(@tmp, "legacy")
    FileUtils.mkdir_p(dir)
    File.write(File.join(dir, "init.rb"), "Sharky.alias 'legacy_cmd', 'printf legacy'\n")
    runtime = Sharky::Runtime.new

    warnings = Sharky::Config.new(dir: dir).load_into(runtime)

    assert_equal "printf legacy", runtime.aliases["legacy_cmd"]
    assert warnings.any? { |warning| warning.include?("legacy") && warning.include?("sharky.rb") }
  end

  def test_ensure_layout_leaves_legacy_only_config_alone
    dir = File.join(@tmp, "legacy-only")
    FileUtils.mkdir_p(dir)
    File.write(File.join(dir, "init.rb"), "# legacy\n")

    refute Sharky::Config.new(dir: dir).ensure_layout!
    refute File.exist?(File.join(dir, "sharky.rb"))
  end

  private

  def build_prompt(runtime)
    shell = ShellStub.new(@tmp, { "HOME" => @tmp }, 0, JobsStub.new([]))
    theme = Sharky::CLI.send(:theme_for, runtime)
    Sharky::CLI.send(:build_prompt, shell, runtime, theme, 0)
  end

  def fake_starship(marker: nil, status: 0)
    path = File.join(@tmp, "starship")
    script = +"#!/bin/sh\n"
    script << "printf 'called\\n' >> #{marker.dump}\n" if marker
    script << "printf '%s\\n' 'STARSHIP> '\n"
    script << "exit #{status}\n"
    File.write(path, script)
    File.chmod(0o755, path)
    path
  end

  def with_no_color
    previous = ENV["NO_COLOR"]
    ENV["NO_COLOR"] = "1"
    yield
  ensure
    ENV["NO_COLOR"] = previous
  end
end
