# frozen_string_literal: true

require_relative "prompt"
require_relative "theme"

module Sharky
  # Small built-in prompt renderers that do not require a TOML layout.
  module PromptPresets
    WIDTH = 80

    module_function

    def fetch(name)
      PRESETS.fetch(name.to_sym)
    end

    def self.[](name)
      fetch(name)
    end

    def render_simple(context)
      theme = current_theme
      path = Prompt.shorten_cwd(context.cwd, context.home)
      segments = [theme.style(:directory, path)]

      summary = git_summary(context)
      segments << theme.style(:git, summary) if summary

      duration = Prompt.format_duration(context.duration_ms)
      segments << theme.style(:hint, duration) if duration

      "#{segments.join(' ')}\n#{theme.style(:prompt, '❯')} "
    end

    def render_ohmyposh(context)
      theme = current_theme
      left = +""
      plain = +""

      add = lambda do |style, text, gap|
        left << gap << theme.style(style, text)
        plain << gap << text
      end

      add.call(:hint, "[]", "")
      add.call(:directory, "sharky", "  ")

      summary = git_summary(context)
      add.call(:git, summary, "  ") if summary

      status = context.status.to_i
      right = theme.style(status.zero? ? :success : :error, "x#{status}")
      right_plain = "x#{status}"

      duration_ms = context.duration_ms.to_i
      if duration_ms.positive?
        duration = "#{duration_ms}ms"
        right << " #{theme.style(:hint, duration)}"
        right_plain << " #{duration}"
      end

      padding = [WIDTH - plain.length - right_plain.length - 1, 1].max
      first_line = "#{left}#{' ' * padding}#{right}"
      path = Prompt.shorten_cwd(context.cwd, context.home)
      second_line = " #{theme.style(:directory, path)}"
      third_line = "#{theme.style(:prompt, '❯')} "

      "#{first_line}\n#{second_line}\n#{third_line}"
    end

    def git_summary(context)
      branch = context.git_branch.to_s
      return nil if branch.empty?

      parts = [context.git_dirty ? "#{branch}*" : branch]
      parts << "↑#{context.git_ahead}" if context.git_ahead.to_i.positive?
      parts << "↓#{context.git_behind}" if context.git_behind.to_i.positive?
      parts << "✚ +#{context.git_staged}" if context.git_staged.to_i.positive?
      parts << "~#{context.git_modified}" if context.git_modified.to_i.positive?
      if context.git_untracked.to_i.positive?
        parts << "|"
        parts << "✚ +#{context.git_untracked}"
      end
      parts.join(" ")
    end

    def current_theme
      styles = Sharky.respond_to?(:theme) ? Sharky.theme : nil
      styles.is_a?(Theme) ? styles : Theme.new(styles || {})
    end

    PRESETS = {
      simple: ->(context) { render_simple(context) },
      ohmyposh: ->(context) { render_ohmyposh(context) }
    }.freeze
  end
end
