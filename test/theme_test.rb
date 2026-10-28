# frozen_string_literal: true

require_relative "test_helper"
require "sharky"
require "sharky/toml"

class ThemeTest < Minitest::Test
  FIXTURE_THEMES = File.join(ROOT, "test", "fixtures", "themes")
  ShellStub = Struct.new(:cwd, :env, :last_status, :jobs)
  JobsStub = Struct.new(:all)

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

  def test_style_wraps_text
    theme = Sharky::Theme.new(error: "31")
    assert_equal "\e[31mboom\e[0m", theme.style(:error, "boom")
  end

  def test_no_color_disables_styles
    old = ENV["NO_COLOR"]
    ENV["NO_COLOR"] = "1"
    theme = Sharky::Theme.new(error: "31")
    assert_equal "boom", theme.style(:error, "boom")
  ensure
    ENV["NO_COLOR"] = old
  end

  def test_sharky_no_color_disables_styles
    old = ENV["SHARKY_NO_COLOR"]
    ENV["SHARKY_NO_COLOR"] = "1"
    theme = Sharky::Theme.new(error: "31")
    assert_equal "boom", theme.style(:error, "boom")
  ensure
    ENV["SHARKY_NO_COLOR"] = old
  end

  def test_merge
    theme = Sharky::Theme.new(error: "31").merge(success: "32")
    assert_equal "\e[32mok\e[0m", theme.style(:success, "ok")
  end

  def test_merge_does_not_mutate_the_original
    theme = Sharky::Theme.new(error: "31")
    merged = theme.merge(error: "32")
    assert_equal "\e[31mboom\e[0m", theme.style(:error, "boom")
    assert_equal "\e[32mboom\e[0m", merged.style(:error, "boom")
  end

  def test_unknown_style_returns_plain_text
    assert_equal "plain", Sharky::Theme.new.style(:missing, "plain")
  end

  def test_default_styles_are_available
    assert_equal "\e[38;5;203mboom\e[0m", Sharky::Theme.new.style(:error, "boom")
  end

  def test_loads_toml_theme
    dir = File.join(@tmp, "config")
    FileUtils.mkdir_p(File.join(dir, "themes"))
    File.write(File.join(dir, "themes", "ocean.toml"), <<~TOML)
      [styles]
      prompt = "31"
      path = "38;2;1;2;3"
    TOML

    with_runtime(dir) do |runtime|
      theme = Sharky.theme(:ocean)
      assert_equal "31", theme[:prompt]
      assert_equal "\e[31mhi\e[0m", Sharky::Theme.new(theme).style(:prompt, "hi")
      assert_equal "38;2;1;2;3", runtime.theme[:path]
    end
  end

  def test_toml_theme_takes_precedence_over_ruby
    dir = File.join(@tmp, "config")
    themes = File.join(dir, "themes")
    FileUtils.mkdir_p(themes)
    File.write(File.join(themes, "ocean.toml"), "prompt = \"31\"\n")
    File.write(File.join(themes, "ocean.rb"), "Sharky.theme = { prompt: \"32\" }\n")

    with_runtime(dir) do
      assert_equal "31", Sharky.theme(:ocean)[:prompt]
    end
  end

  def test_invalid_toml_theme_raises
    dir = File.join(@tmp, "config")
    themes = File.join(dir, "themes")
    FileUtils.mkdir_p(themes)
    File.write(File.join(themes, "bad.toml"), "prompt = 1\n")

    with_runtime(dir) do
      error = assert_raises(Sharky::Toml::ParseError) { Sharky.theme(:bad) }
      assert_includes error.message, "bad.toml"
    end
  end

  def test_ruby_theme_fallback_still_works
    dir = File.join(@tmp, "config")
    themes = File.join(dir, "themes")
    FileUtils.mkdir_p(themes)
    File.write(File.join(themes, "legacy.rb"), "Sharky.theme = { prompt: \"32\" }\n")

    with_runtime(dir) do
      assert_equal "32", Sharky.theme(:legacy)[:prompt]
    end
  end

  def test_toml_theme_layout_installs_and_renders
    dir = File.join(@tmp, "config")
    install_fixture_theme(:layout, dir)

    with_runtime(dir) do |runtime|
      theme = Sharky.theme(:layout)

      assert_respond_to runtime, :theme_layout
      assert_instance_of Sharky::PromptLayout, runtime.theme_layout
      assert_equal "137;180;250", theme[:arch]

      output = with_no_color { build_prompt(runtime) }
      expected = "layout #{Sharky::Prompt.os_name} #{Sharky::Prompt.arch} #{File.basename(@tmp)}> "
      assert_equal expected, output
    end
  end

  def test_loading_colors_only_theme_clears_layout
    dir = File.join(@tmp, "config")
    install_fixture_theme(:layout, dir)
    FileUtils.mkdir_p(File.join(dir, "themes"))
    File.write(File.join(dir, "themes", "ocean.toml"), <<~TOML)
      [styles]
      prompt = "31"
    TOML

    with_runtime(dir) do |runtime|
      Sharky.theme(:layout)
      assert_instance_of Sharky::PromptLayout, runtime.theme_layout

      theme = Sharky.theme(:ocean)

      assert_equal "31", theme[:prompt]
      assert_nil runtime.theme_layout
    end
  end

  def test_ruby_theme_clears_previous_layout
    dir = File.join(@tmp, "config")
    install_fixture_theme(:layout, dir)
    FileUtils.mkdir_p(File.join(dir, "themes"))
    File.write(File.join(dir, "themes", "legacy.rb"), "Sharky.theme = { prompt: \"32\" }\n")

    with_runtime(dir) do |runtime|
      Sharky.theme(:layout)
      assert_instance_of Sharky::PromptLayout, runtime.theme_layout

      assert_equal "32", Sharky.theme(:legacy)[:prompt]
      assert_nil runtime.theme_layout
    end
  end

  def test_reload_clears_and_reinstalls_toml_layout
    dir = File.join(@tmp, "config")
    install_fixture_theme(:layout, dir)
    write_config(dir, 'Sharky.theme :layout')

    with_runtime(dir) do |runtime|
      runtime.reload_config
      assert_instance_of Sharky::PromptLayout, runtime.theme_layout
      assert_includes with_no_color { build_prompt(runtime) }, "layout "

      write_config(dir, "")
      runtime.reload_config
      assert_nil runtime.theme_layout
      assert_includes with_no_color { build_prompt(runtime) }, "❯"

      write_config(dir, 'Sharky.theme :layout')
      runtime.reload_config
      assert_instance_of Sharky::PromptLayout, runtime.theme_layout
    end
  end

  def test_cli_build_prompt_uses_layout_and_populates_context
    dir = File.join(@tmp, "config")
    FileUtils.mkdir_p(File.join(dir, "themes"))
    File.write(File.join(dir, "themes", "layout.toml"), layout_theme(
      "{os}|{arch}|{branch}|{ahead}|{behind}|{staged}|{modified}|{untracked}"
    ))
    fake_git = Struct.new(:info).new(
      {
        branch: "main",
        dirty: true,
        ahead: 2,
        behind: 1,
        staged: 3,
        modified: 4,
        untracked: 5
      }
    )

    with_runtime(dir) do |runtime|
      Sharky.theme(:layout)

      output = with_no_color do
        Sharky::Prompt.stub(:os_name, "TestOS") do
          Sharky::Prompt.stub(:arch, "testarch") do
            Sharky::Prompt::Git.stub(:new, fake_git) do
              build_prompt(runtime)
            end
          end
        end
      end

      assert_equal "TestOS|testarch|main|2|1|3|4|5> ", output
    end
  end

  def test_cli_prompt_overrides_layout
    dir = File.join(@tmp, "config")
    install_fixture_theme(:layout, dir)

    with_runtime(dir) do |runtime|
      Sharky.theme(:layout)
      Sharky.prompt = ->(_context) { "ruby> " }

      assert_equal "ruby> ", with_no_color { build_prompt(runtime) }
    end
  end

  private

  def install_fixture_theme(name, dir)
    themes = File.join(dir, "themes")
    FileUtils.mkdir_p(themes)
    FileUtils.cp(File.join(FIXTURE_THEMES, "#{name}.toml"), File.join(themes, "#{name}.toml"))
  end

  def write_config(dir, body)
    FileUtils.mkdir_p(dir)
    File.write(File.join(dir, "sharky.rb"), body)
  end

  def layout_theme(text)
    <<~TOML
      [prompt]
      separator = "none"
      padding = 0

      [styles]
      arch = "137;180;250"
      ink = "30;30;46"

      [[lines]]
      suffix = "> "

      [[segments]]
      line = 1
      text = "#{text}"
      bg = "arch"
      fg = "ink"
      when = "always"
    TOML
  end

  def build_prompt(runtime)
    shell = ShellStub.new(@tmp, { "HOME" => @tmp }, 0, JobsStub.new([]))
    theme = Sharky::CLI.send(:theme_for, runtime)
    Sharky::CLI.send(:build_prompt, shell, runtime, theme, 0)
  end

  def with_no_color
    old = ENV["NO_COLOR"]
    ENV["NO_COLOR"] = "1"
    yield
  ensure
    ENV["NO_COLOR"] = old
  end

  def with_runtime(dir)
    previous = Sharky.runtime
    runtime = Sharky::Runtime.new
    runtime.config_dir = dir
    Sharky.runtime = runtime
    yield runtime
  ensure
    Sharky.runtime = previous
  end
end
