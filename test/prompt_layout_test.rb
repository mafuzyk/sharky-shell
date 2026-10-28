# frozen_string_literal: true

require_relative "test_helper"
require "sharky"
require "sharky/toml"
require "sharky/prompt_layout"

class PromptLayoutTest < Minitest::Test
  STYLES = {
    arch: "137;180;250",
    pink: "245;194;231",
    white: "245;245;245",
    blue: "137;180;250",
    ink: "30;30;46",
    night: "24;24;32",
    fire1: "255;95;31",
    fire2: "255;176;0",
    fire3: "255;45;85",
    prompt_fg: "245;245;245"
  }.freeze

  LAYOUT = <<~TOML
    [prompt]
    separator = "fire"
    leading = "square_fade"
    trailing = "point_fade"
    padding = 1

    [styles]
    arch = "137;180;250"
    pink = "245;194;231"
    white = "245;245;245"
    blue = "137;180;250"
    ink = "30;30;46"
    night = "24;24;32"
    fire1 = "255;95;31"
    fire2 = "255;176;0"
    fire3 = "255;45;85"
    prompt_fg = "245;245;245"

    [[lines]]
    suffix = ""

    [[lines]]
    suffix = "> "

    [[segments]]
    line = 1
    icon = "A"
    text = "{os}"
    bg = "arch"
    fg = "ink"
    when = "always"

    [[segments]]
    line = 1
    icon = "B"
    text = "{arch}"
    bg = "pink"
    fg = "ink"
    when = "always"

    [[segments]]
    line = 1
    icon = "G"
    text = "{git}"
    bg = "pink"
    fg = "ink"
    when = "git"

    [[segments]]
    line = 2
    text = "second"
    bg = "blue"
    fg = "white"
    when = "always"
  TOML

  def setup
    super
    @old_no_color = ENV["NO_COLOR"]
    @old_sharky_no_color = ENV["SHARKY_NO_COLOR"]
    ENV.delete("NO_COLOR")
    ENV.delete("SHARKY_NO_COLOR")
  end

  def teardown
    ENV["NO_COLOR"] = @old_no_color
    ENV["SHARKY_NO_COLOR"] = @old_sharky_no_color
    super
  end

  def test_renders_two_lines_in_expected_order_with_suffix
    out = with_no_color do
      layout.render(context(os: "Arch", arch: "x86_64"), theme: theme)
    end

    assert_equal "AArch  Bx86_64\nsecond> ", out
  end

  def test_substitutes_every_documented_variable
    cwd = File.join(@tmp, "src", "sharky")
    ctx = context(
      cwd: cwd, home: @tmp, os: "Arch", arch: "x86_64",
      user: "mafu", host: "box", git_branch: "main",
      git_ahead: 2, git_behind: 1, git_staged: 3, git_modified: 4,
      git_untracked: 5, git_dirty: true, status: 7, duration_ms: 1234,
      jobs_count: 2, ssh: true
    )
    template = [
      "{os}", "{arch}", "{user}", "{host}", "{cwd}", "{cwd_full}",
      "{cwd_base}", "{branch}", "{ahead}", "{behind}", "{staged}",
      "{modified}", "{untracked}", "{git}", "{status}", "{duration}",
      "{jobs}", "{ssh}", "{dirty}"
    ].join("|")
    out = with_no_color do
      layout(one_line_layout(template)).render(ctx, theme: theme)
    end

    expected = [
      "Arch", "x86_64", "mafu", "box", "~/src/sharky", cwd, "sharky",
      "main", "2", "1", "3", "4", "5", "main • ↑2 ↓1", "7", "1.23s",
      "2", "1", "1"
    ].join("|")
    assert_equal expected, out
  end

  def test_unknown_variables_are_empty_and_double_braces_are_literal
    out = with_no_color do
      layout(one_line_layout("{missing}|{{literal}}|{os}"))
        .render(context(os: "Arch"), theme: theme)
    end

    assert_equal "|{literal}|Arch", out
  end

  def test_long_cwd_is_shortened_to_home_and_basename
    cwd = File.join(@tmp, "a" * 40, "project")
    out = with_no_color do
      layout(one_line_layout("{cwd}|{cwd_full}|{cwd_base}"))
        .render(context(cwd: cwd, home: @tmp), theme: theme)
    end

    assert_equal "~/…/project|#{cwd}|project", out
  end

  def test_cwd_shortens_when_it_would_crowd_the_prompt
    cwd = File.join(@tmp, "a" * 20, "project")
    out = with_no_color do
      layout(one_line_layout("{cwd}")).render(context(cwd: cwd, home: @tmp), theme: theme)
    end

    assert_equal "~/…/project", out
  end

  def test_cwd_stays_absolute_when_the_context_has_no_home
    out = with_no_color do
      layout(one_line_layout("{cwd}")).render(context(cwd: "/usr", home: nil), theme: theme)
    end

    assert_equal "/usr", out
  end

  def test_arrow_separator_uses_segment_backgrounds
    out = layout(two_segment_toml).render(context, theme: theme)

    assert_equal(
      "\e[38;2;30;30;46;48;2;137;180;250m A " \
      "\e[38;2;137;180;250;48;2;245;194;231m" \
      "\e[38;2;30;30;46;48;2;245;194;231m B \e[0m",
      out
    )
  end

  def test_full_sgr_styles_are_converted_to_the_needed_color_role
    legacy = theme(arch: "38;5;31", pink: "38;5;205", ink: "38;5;232")
    out = layout(two_segment_toml).render(context, theme: legacy)

    assert_equal(
      "\e[38;5;232;48;5;31m A \e[38;5;31;48;5;205m" \
      "\e[38;5;232;48;5;205m B \e[0m",
      out
    )
  end

  def test_fire_and_none_separators_are_distinct
    arrow = layout(two_segment_toml(separator: "arrow")).render(context, theme: theme)
    fire = layout(two_segment_toml(separator: "fire")).render(context, theme: theme)
    none = layout(two_segment_toml(separator: "none")).render(context, theme: theme)

    assert_equal 3, [arrow, fire, none].uniq.length
    assert_includes arrow, ""
    refute_includes none, ""
    assert_includes fire, "\e[38;2;137;180;250;48;2;255;95;31m "
    assert_includes fire, "\e[38;2;255;95;31;48;2;255;176;0m"
    assert_includes fire, "\e[38;2;255;176;0;48;2;255;45;85m "
  end

  def test_gradient_separator_blends_neighbouring_backgrounds
    out = layout(two_segment_toml(separator: "gradient")).render(context, theme: theme)

    assert_includes out, "\e[48;2;173;185;244m "
    assert_includes out, "\e[48;2;208;189;237m "
    assert_includes out, "\e[38;2;234;193;233;48;2;245;194;231m"
  end

  def test_gradient_separator_uses_a_dark_arrow_for_equal_backgrounds
    toml = two_segment_toml(
      separator: "gradient", first_bg: "white", second_bg: "white"
    )
    out = layout(toml).render(context, theme: theme)

    assert_includes out, "\e[38;2;30;30;46;48;2;245;245;245m"
  end

  def test_square_and_point_fades_are_distinct
    square = layout(two_segment_toml(separator: "none", leading: "square_fade"))
      .render(context, theme: theme)
    point = layout(two_segment_toml(separator: "none", trailing: "point_fade"))
      .render(context, theme: theme)

    assert square.start_with?("\e[48;2;75;94;130m ")
    assert point.end_with?(
      "\e[48;2;146;118;141m \e[0m\e[38;2;146;118;141m\e[0m"
    )
    refute_equal square, point
  end

  def test_line_without_visible_segments_stays_empty
    data = <<~TOML
      [[segments]]
      text = "hidden"
      bg = "arch"
      fg = "ink"
      when = "git"
    TOML

    out = layout(data).render(context, theme: theme)

    assert_equal "", out
  end

  def test_suffix_is_rendered_after_the_last_transition
    out = layout(two_segment_toml(suffix: "> ")).render(context, theme: theme)

    assert out.end_with?("\e[0m\e[38;2;245;245;245m> \e[0m")
  end

  def test_layout_defaults_match_the_spec
    out = layout(two_segment_toml(prompt_fields: {}, suffix: ""))
      .render(context, theme: theme)
    default = layout(two_segment_toml(separator: "arrow")).render(context, theme: theme)

    assert_equal default, out
  end

  def test_fire_and_night_fall_back_when_theme_omits_them
    partial = theme(
      arch: STYLES[:arch], pink: STYLES[:pink], ink: STYLES[:ink]
    )
    fire = layout(two_segment_toml(separator: "fire")).render(context, theme: partial)
    point = layout(two_segment_toml(separator: "none", trailing: "point_fade"))
      .render(context, theme: partial)

    assert_includes fire, "48;2;255;95;31"
    assert_includes fire, "48;2;255;176;0"
    assert_includes fire, "48;2;255;45;85"
    assert point.end_with?(
      "\e[48;2;146;118;141m \e[0m\e[38;2;146;118;141m\e[0m"
    )

    suffix = layout(two_segment_toml(suffix: "> ")).render(context, theme: partial)
    assert suffix.end_with?("\e[0m\e[38;2;245;245;245m> \e[0m")
  end

  def test_color_can_be_disabled_with_either_environment_variable
    renderer = layout(LAYOUT)
    ctx = context(os: "Arch", arch: "x86_64")
    colored = renderer.render(ctx, theme: theme)
    assert_includes colored, "\e["

    [["NO_COLOR", "1"], ["SHARKY_NO_COLOR", "1"]].each do |name, value|
      old = ENV[name]
      ENV[name] = value
      plain = renderer.render(ctx, theme: theme)
      assert_equal "AArch  Bx86_64\nsecond> ", plain
      refute_includes plain, "\e["
    ensure
      ENV[name] = old
    end
  end

  def test_git_variable_is_empty_outside_a_repository
    out = with_no_color do
      layout(one_line_layout("{git}")).render(context(git_branch: nil), theme: theme)
    end

    assert_equal "", out
  end

  def test_every_documented_when_condition_filters_segments
    conditions = %w[
      always git ssh jobs error success dirty ahead behind staged modified
      untracked
    ]
    toml = conditions_layout(conditions + ["bogus"])
    active = context(
      git_branch: "main", ssh: true, jobs_count: 2, status: 1,
      git_dirty: true, git_ahead: 1, git_behind: 1, git_staged: 1,
      git_modified: 1, git_untracked: 1
    )
    clean = context(ssh: false, jobs_count: 0, status: 0, git_dirty: nil)

    active_out = with_no_color { layout(toml).render(active, theme: theme) }
    clean_out = with_no_color { layout(toml).render(clean, theme: theme) }

    expected_active = (conditions - ["success"]).join("  ") + "  default"
    assert_equal expected_active, active_out
    assert_equal "always  success  default", clean_out
  end

  def test_implicit_lines_appear_in_numeric_order
    toml = <<~TOML
      [prompt]
      padding = 1

      [[lines]]
      suffix = ""

      [[segments]]
      line = 1
      text = "one"
      bg = "arch"
      fg = "ink"

      [[segments]]
      line = 3
      text = "three"
      bg = "arch"
      fg = "ink"

      [[segments]]
      line = 2
      text = "two"
      bg = "arch"
      fg = "ink"
    TOML

    out = with_no_color { layout(toml).render(context, theme: theme) }

    assert_equal "one\ntwo\nthree", out
  end

  private

  def layout(toml = LAYOUT)
    Sharky::PromptLayout.from_toml(Sharky::Toml.parse(toml))
  end

  def theme(styles = STYLES)
    Sharky::Theme.new(styles)
  end

  def context(overrides = {})
    defaults = {
      cwd: @tmp, home: @tmp, status: 0, duration_ms: 0,
      git_branch: nil, git_dirty: nil, git_ahead: 0, git_behind: 0,
      git_staged: 0, git_modified: 0, git_untracked: 0,
      os: "Arch", arch: "x86_64", ssh: false, user: "u", host: "h",
      jobs_count: 0, time: Time.now
    }
    Sharky::Prompt::Context.new(**defaults.merge(overrides))
  end

  def two_segment_toml(separator: "arrow", leading: "none", trailing: "none",
                      padding: 1, suffix: "", prompt_fields: nil,
                      first_bg: "arch", second_bg: "pink")
    fields = prompt_fields || {
      "separator" => separator,
      "leading" => leading,
      "trailing" => trailing,
      "padding" => padding
    }
    prompt_body = fields.map { |key, value| "#{key} = #{toml_value(value)}" }.join("\n")

    <<~TOML
      [prompt]
      #{prompt_body}

      [[lines]]
      suffix = #{suffix.inspect}

      [[segments]]
      line = 1
      text = "A"
      bg = #{first_bg.inspect}
      fg = "ink"

      [[segments]]
      line = 1
      text = "B"
      bg = #{second_bg.inspect}
      fg = "ink"
    TOML
  end

  def toml_value(value)
    value.is_a?(Integer) ? value.to_s : value.inspect
  end

  def one_line_layout(template)
    <<~TOML
      [prompt]
      padding = 1

      [[lines]]
      suffix = ""

      [[segments]]
      text = "#{template}"
      bg = "arch"
      fg = "ink"
      when = "always"
    TOML
  end

  def conditions_layout(conditions)
    segments = conditions.map do |condition|
      text = condition == "bogus" ? "bogus" : condition
      <<~TOML
        [[segments]]
        text = "#{text}"
        bg = "arch"
        fg = "ink"
        when = "#{condition}"
      TOML
    end.join
    header = <<~TOML
      [prompt]
      padding = 1

      [[lines]]
      suffix = ""
    TOML
    default = <<~TOML
      [[segments]]
      text = "default"
      bg = "arch"
      fg = "ink"
    TOML

    "#{header}#{segments}#{default}"
  end

  def with_no_color
    old = ENV["NO_COLOR"]
    ENV["NO_COLOR"] = "1"
    yield
  ensure
    ENV["NO_COLOR"] = old
  end
end
