require_relative "test_helper"
require "sharky"
require "yaml"

class ReleaseTest < Minitest::Test
  def test_version_is_1_0_1
    assert_equal "1.0.1", Sharky::VERSION
    out, _err, status = run_sharky("--version")
    assert status.success?
    assert_equal "sharky 1.0.1\n", out
  end

  def test_result_copies_and_freezes_environment_names_and_values
    name = +"SHARKY_RESULT_TEST"
    value = +"before"
    delta = {}.compare_by_identity
    delta[name] = value
    delta[+"REMOVED"] = nil
    result = Sharky::Result.new(status: 0, stdout: "", stderr: "", duration_ms: 0, cwd: @tmp, env_delta: delta)

    name.replace("RENAMED")
    value.replace("after")

    assert_equal ["SHARKY_RESULT_TEST", "REMOVED"], result.env_delta.keys
    assert_equal "before", result.env_delta["SHARKY_RESULT_TEST"]
    assert result.env_delta.key?("REMOVED")
    assert_nil result.env_delta["REMOVED"]
    assert_predicate result.env_delta, :frozen?
    assert_predicate result.env_delta.keys.first, :frozen?
    assert_raises(FrozenError) { result.env_delta["SHARKY_RESULT_TEST"].replace("mutated") }
    refute_predicate value, :frozen?
  end

  def test_result_environment_delta_is_an_immutable_snapshot_of_session_environment
    session = Sharky::Session.new(env: { "PATH" => "/usr/bin:/bin", "HOME" => @tmp }, cwd: @tmp)
    result = session.run("export SHARKY_RESULT_TEST=before")
    session.env["SHARKY_RESULT_TEST"].replace("after")

    assert_equal "before", result.env_delta["SHARKY_RESULT_TEST"]
    refute_same session.env["SHARKY_RESULT_TEST"], result.env_delta["SHARKY_RESULT_TEST"]
    assert_predicate result.env_delta, :frozen?
    assert_predicate result.env_delta.keys.first, :frozen?
    assert_raises(FrozenError) { result.env_delta["SHARKY_RESULT_TEST"].replace("mutated") }
    assert_equal "after", session.env["SHARKY_RESULT_TEST"]
  end

  def test_changelog_has_release_entry_and_limitations
    changelog = File.read(File.join(ROOT, "CHANGELOG.md"))
    assert_includes changelog, "## [1.0.0]"
    assert_includes changelog, "Known limitations"
  end

  def test_soak_rejects_editor_echo_without_command_output
    out, err, status = run_soak_with_fake_terminal(emit_output: false, redraw: true)

    refute status.success?, "soak accepted editor echo as command output: #{out}#{err}"
    assert_match(/pty round 0: timed out/, err)
  end

  def test_soak_requires_a_new_prompt_after_command_output
    out, err, status = run_soak_with_fake_terminal(emit_output: true, redraw: false)

    refute status.success?, "soak accepted command output without a new prompt: #{out}#{err}"
    assert_match(/pty round 0: timed out/, err)
  end

  def test_ci_warning_check_fails_when_a_successful_test_run_emits_a_warning
    out, err, status = run_warning_check('warn "fixture.rb:1: warning: unused variable - marker"')

    refute status.success?, "the warning check accepted a Ruby warning: #{out}#{err}"
    assert_includes err, "warning: unused variable - marker"
  end

  def test_ci_warning_check_accepts_clean_test_runs
    out, err, status = run_warning_check('puts "clean fixture"')

    assert status.success?, "clean warning check failed: #{out}#{err}"
    assert_includes out, "clean fixture"
  end

  def test_ci_warning_check_preserves_test_failure_status
    _out, _err, status = run_warning_check("exit 7")

    assert_equal 7, status.exitstatus
  end

  def test_arch_git_source_declares_git_as_a_build_dependency
    out, err, status = Open3.capture3(
      "bash", "-c", 'source "$1"; printf "%s\\n" "${makedepends[@]}"', "package-test",
      File.join(ROOT, "packaging", "PKGBUILD")
    )

    assert status.success?, err
    assert_includes out.lines.map(&:strip), "git", "a clean Arch build needs git to fetch the source"
  end

  def test_man_and_pkgbuild_versions_match
    man = File.read(File.join(ROOT, "man", "sharky.1"))
    pkgbuild = File.read(File.join(ROOT, "packaging", "PKGBUILD"))
    assert_includes man, "sharky 1.0.1"
    assert_includes pkgbuild, "pkgver=1.0.1"
  end

  private

  def run_soak_with_fake_terminal(emit_output:, redraw:)
    FileUtils.mkdir_p([File.join(@tmp, "bench"), File.join(@tmp, "bin")])
    script = File.join(@tmp, "bench", "soak.rb")
    FileUtils.cp(File.join(ROOT, "bench", "soak.rb"), script)
    fake = File.join(@tmp, "bin", "sharky")
    File.write(fake, <<~RUBY)
      #!#{RbConfig.ruby}
      exec(#{BIN.dump}, *ARGV) if ARGV.include?("-c")
      STDOUT.sync = true
      STDOUT.write("❯ ")
      STDIN.each_line do |line|
        exit 0 if line.strip == "exit 0"
        if #{emit_output}
          STDOUT.write(IO.popen(["/bin/bash", "-c", line], &:read))
        else
          STDOUT.write(line)
        end
        STDOUT.write("❯ ") if #{redraw}
      end
    RUBY
    File.chmod(0755, fake)
    Open3.capture3(RbConfig.ruby, script)
  end

  def run_warning_check(fixture)
    workflow = YAML.load_file(File.join(ROOT, ".github", "workflows", "ci.yml"))
    step = workflow.fetch("jobs").fetch("test").fetch("steps").find { |entry| entry["name"] == "Warning check" }
    FileUtils.mkdir_p(File.join(@tmp, "test"))
    File.write(File.join(@tmp, "test", "run.rb"), fixture)
    Open3.capture3("bash", "-e", "-o", "pipefail", "-c", step.fetch("run"), chdir: @tmp)
  end
end
