# frozen_string_literal: true

require_relative "test_helper"
require "sharky"
require "sharky/prompt_presets"

class PromptPresetsTest < Minitest::Test
  def setup
    super
    @previous_runtime = Sharky.runtime
    @previous_no_color = ENV["NO_COLOR"]
    @previous_sharky_no_color = ENV["SHARKY_NO_COLOR"]
    ENV.delete("NO_COLOR")
    ENV.delete("SHARKY_NO_COLOR")
    Sharky.runtime = Sharky::Runtime.new
  end

  def teardown
    Sharky.runtime = @previous_runtime
    restore_env("NO_COLOR", @previous_no_color)
    restore_env("SHARKY_NO_COLOR", @previous_sharky_no_color)
    super
  end

  def test_simple_is_two_lines_with_path_git_summary_and_duration
    ENV["NO_COLOR"] = "1"
    out = Sharky::PromptPresets.fetch(:simple).call(context)

    lines = out.split("\n", -1)
    assert_equal 2, lines.length
    assert_equal "~/project main* ↑2 ↓1 ✚ +3 ~4 | ✚ +5 2.5s", lines[0]
    assert_equal "❯ ", lines[1]
  end

  def test_simple_omits_git_summary_outside_a_repository
    ENV["NO_COLOR"] = "1"
    out = Sharky::PromptPresets.fetch(:simple).call(
      context(
        git_branch: nil,
        git_dirty: nil,
        git_ahead: nil,
        git_behind: nil,
        git_staged: nil,
        git_modified: nil,
        git_untracked: nil
      )
    )

    assert_equal "~/project 2.5s\n❯ ", out
  end

  def test_simple_omits_short_commands
    ENV["NO_COLOR"] = "1"
    out = Sharky::PromptPresets.fetch(:simple).call(context(duration_ms: 1999))

    assert_equal "~/project main* ↑2 ↓1 ✚ +3 ~4 | ✚ +5\n❯ ", out
  end

  def test_ohmyposh_is_three_lines_with_badges_path_and_prompt
    ENV["NO_COLOR"] = "1"
    out = Sharky::PromptPresets.fetch(:ohmyposh).call(context)

    lines = out.split("\n", -1)
    assert_equal 3, lines.length
    assert lines[0].start_with?("[]  sharky  main* ↑2 ↓1 ✚ +3 ~4 | ✚ +5")
    assert lines[0].end_with?("x0 2500ms")
    assert_equal " ~/project", lines[1]
    assert_equal "❯ ", lines[2]
  end

  def test_ohmyposh_omits_git_badges_outside_a_repository
    ENV["NO_COLOR"] = "1"
    out = Sharky::PromptPresets.fetch(:ohmyposh).call(
      context(
        git_branch: nil,
        git_dirty: nil,
        git_ahead: nil,
        git_behind: nil,
        git_staged: nil,
        git_modified: nil,
        git_untracked: nil
      )
    )

    lines = out.split("\n", -1)
    assert lines[0].start_with?("[]  sharky")
    refute_match(/main|↑|↓|✚|~/, lines[0])
    assert_equal " ~/project", lines[1]
    assert_equal "❯ ", lines[2]
  end

  def test_presets_use_theme_colors
    Sharky.runtime.theme = {
      directory: "31",
      git: "32",
      hint: "33",
      prompt: "34",
      success: "35",
      error: "36"
    }

    simple = Sharky::PromptPresets.fetch(:simple).call(context)
    assert_includes simple, "\e[31m~/project\e[0m"
    assert_includes simple, "\e[32mmain* ↑2 ↓1 ✚ +3 ~4 | ✚ +5\e[0m"
    assert_includes simple, "\e[33m2.5s\e[0m"
    assert_includes simple, "\e[34m❯\e[0m"

    ohmyposh = Sharky::PromptPresets.fetch(:ohmyposh).call(context)
    assert_includes ohmyposh, "\e[33m[]\e[0m"
    assert_includes ohmyposh, "\e[31msharky\e[0m"
    assert_includes ohmyposh, "\e[32mmain* ↑2 ↓1 ✚ +3 ~4 | ✚ +5\e[0m"
    assert_includes ohmyposh, "\e[35mx0\e[0m"
    assert_includes ohmyposh, "\e[33m2500ms\e[0m"
    assert_includes ohmyposh, "\e[31m~/project\e[0m"
    assert_includes ohmyposh, "\e[34m❯\e[0m"
  end

  def test_presets_render_plain_text_when_color_is_disabled
    ENV["NO_COLOR"] = "1"

    simple = Sharky::PromptPresets.fetch(:simple).call(context)
    ohmyposh = Sharky::PromptPresets.fetch(:ohmyposh).call(context)

    refute_match(/\e\[/, simple)
    refute_match(/\e\[/, ohmyposh)
  end

  def test_presets_do_not_spawn_processes
    Process.stub(:spawn, ->(*) { flunk "preset spawned a process" }) do
      assert_includes Sharky::PromptPresets.fetch(:simple).call(context), "main*"
      assert_includes Sharky::PromptPresets.fetch(:ohmyposh).call(context), "sharky"
    end
  end

  def test_unknown_preset_raises_key_error
    assert_raises(KeyError) { Sharky::PromptPresets.fetch(:unknown) }
  end

  def test_bracket_lookup_is_supported
    assert_same Sharky::PromptPresets.fetch(:simple), Sharky::PromptPresets[:simple]
  end

  private

  def context(**overrides)
    Sharky::Prompt::Context.new(
      cwd: "/home/u/project",
      home: "/home/u",
      status: 0,
      duration_ms: 2500,
      git_branch: "main",
      git_dirty: true,
      git_ahead: 2,
      git_behind: 1,
      git_staged: 3,
      git_modified: 4,
      git_untracked: 5,
      ssh: false,
      user: "u",
      host: "h",
      jobs_count: 0,
      time: Time.at(0),
      **overrides
    )
  end

  def restore_env(name, value)
    value.nil? ? ENV.delete(name) : ENV[name] = value
  end
end
