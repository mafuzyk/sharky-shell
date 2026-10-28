# frozen_string_literal: true

require_relative "test_helper"
require "json"
require "rbconfig"
require "sharky"
require "sharky/starship"

class StarshipTest < Minitest::Test
  FakeContext = Struct.new(:cwd, :status, :duration_ms, :jobs_count, keyword_init: true)

  def setup
    super
    @record = File.join(@tmp, "record.json")
    @fake = File.join(@tmp, "fake-starship")
    @context = FakeContext.new(cwd: @tmp, status: 7, duration_ms: 1234, jobs_count: 2)
    @old_shell = ENV["STARSHIP_SHELL"]
    @old_config = ENV["STARSHIP_CONFIG"]
    ENV.delete("STARSHIP_CONFIG")
  end

  def teardown
    ENV["STARSHIP_SHELL"] = @old_shell
    ENV["STARSHIP_CONFIG"] = @old_config
    super
  end

  def test_passes_status_duration_and_jobs_to_the_binary
    write_fake

    Sharky::Starship.prompt(@context, command: @fake)

    assert_equal(
      ["prompt", "--status", "7", "--cmd-duration", "1234", "--jobs", "2"],
      record["argv"]
    )
  end

  def test_sets_shell_config_and_working_directory
    config = File.join(@tmp, "starship.toml")
    write_fake
    ENV["STARSHIP_SHELL"] = "bash"

    Sharky::Starship.prompt(@context, command: @fake, config: config)

    assert_equal "sharky", record["shell"]
    assert_equal config, record["config"]
    assert_equal File.realpath(@tmp), File.realpath(record["cwd"])
  end

  def test_omits_config_when_not_given
    write_fake

    Sharky::Starship.prompt(@context, command: @fake)

    assert_nil record["config"]
  end

  def test_returns_multiline_ansi_output_and_chomps_one_newline
    write_fake(output: "\e[31mfirst\e[0m\nsecond\n\n")

    assert_equal "\e[31mfirst\e[0m\nsecond\n", Sharky::Starship.prompt(@context, command: @fake)
  end

  def test_command_is_not_run_through_a_shell
    marker = File.join(@tmp, "marker")
    fake = File.join(@tmp, "fake; touch marker")
    write_fake(fake)

    Sharky::Starship.prompt(@context, command: fake)

    refute File.exist?(marker), "the command was interpreted by a shell"
  end

  def test_available_checks_the_executable_without_spawning
    directory = File.join(@tmp, "bin")
    FileUtils.mkdir_p(directory)
    write_fake(File.join(directory, "starship"))
    old_path = ENV["PATH"]
    ENV["PATH"] = [directory, old_path].compact.join(File::PATH_SEPARATOR)

    assert Sharky::Starship.available?
    refute File.exist?(@record), "available? spawned the binary"
  ensure
    ENV["PATH"] = old_path
  end

  def test_missing_or_non_executable_binary_is_unavailable_and_prompt_raises
    missing = File.join(@tmp, "missing-starship")
    plain = File.join(@tmp, "not-executable")
    File.write(plain, "")

    refute Sharky::Starship.available?(command: missing)
    refute Sharky::Starship.available?(command: plain)
    assert_raises(Sharky::Starship::Error) { Sharky::Starship.prompt(@context, command: missing) }
  end

  def test_nonzero_exit_raises_an_error_with_stderr
    write_fake(body: <<~RUBY)
      #!/usr/bin/env ruby
      STDERR.puts "boom"
      exit 3
    RUBY

    error = assert_raises(Sharky::Starship::Error) do
      Sharky::Starship.prompt(@context, command: @fake)
    end

    assert_match(/3/, error.message)
    assert_match(/boom/, error.message)
  end

  def test_timeout_kills_the_process_group
    write_fake(body: <<~RUBY)
      #!/usr/bin/env ruby
      require "json"
      child = Process.spawn("sleep", "5")
      File.write(#{@record.dump}, JSON.generate("child" => child))
      sleep 5
    RUBY
    started = monotonic

    error = assert_raises(Sharky::Starship::Error) do
      Sharky::Starship.prompt(@context, command: @fake, timeout: 0.4)
    end

    assert_match(/timed out/, error.message)
    assert_operator monotonic - started, :<, 2
    assert wait_until { !alive?(record["child"]) }
  end

  def test_requiring_the_module_does_not_spawn_starship
    directory = File.join(@tmp, "path")
    FileUtils.mkdir_p(directory)
    write_fake(File.join(directory, "starship"))
    env = { "PATH" => [directory, ENV["PATH"]].compact.join(File::PATH_SEPARATOR) }

    _out, err, status = Open3.capture3(
      env, RbConfig.ruby, "-I", File.join(ROOT, "lib"), "-e", "require 'sharky/starship'"
    )

    assert status.success?, err
    refute File.exist?(@record), "requiring the module spawned starship"
  end

  private

  def write_fake(path = @fake, output: "\e[31mfirst\e[0m\nsecond\n", body: nil)
    script = body || <<~RUBY
      #!/usr/bin/env ruby
      require "json"
      File.write(#{@record.dump}, JSON.generate(
        "argv" => ARGV,
        "shell" => ENV["STARSHIP_SHELL"],
        "config" => ENV["STARSHIP_CONFIG"],
        "cwd" => Dir.pwd
      ))
      STDOUT.write(#{output.dump})
    RUBY
    File.write(path, script)
    File.chmod(0o755, path)
    path
  end

  def record
    JSON.parse(File.read(@record))
  end

  def monotonic
    Process.clock_gettime(Process::CLOCK_MONOTONIC)
  end

  def alive?(pid)
    Process.kill(0, pid)
    true
  rescue Errno::ESRCH
    false
  rescue Errno::EPERM
    true
  end

  def wait_until(timeout = 1)
    deadline = monotonic + timeout
    loop do
      return true if yield
      return false if monotonic >= deadline

      sleep 0.01
    end
  end
end
