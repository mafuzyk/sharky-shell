# frozen_string_literal: true

module Sharky
  # Renders the prompt layout tables from a theme TOML file.
  class PromptLayout
    CWD_MAX = 22
    ARROW = ""
    SQUARE_FADE = "▌"
    RESET = "\e[0m"
    LEADING_MIX = 0.55
    TRAILING_MIX = 0.45
    FALLBACK_STYLES = {
      "night" => "24;24;32",
      "fire1" => "255;95;31",
      "fire2" => "255;176;0",
      "fire3" => "255;45;85",
      "prompt_fg" => "245;245;245"
    }.freeze

    def self.from_toml(data)
      new(data)
    end

    def initialize(data)
      @prompt = table(data, "prompt")
      @line_defs = array(data, "lines")
      @segments = array(data, "segments")
      @separator = value(@prompt, "separator", "arrow").to_s
      @leading = value(@prompt, "leading", "none").to_s
      @trailing = value(@prompt, "trailing", "none").to_s
      @padding = padding_value
    end

    def render(context, theme:)
      @theme = theme
      @styles = theme.respond_to?(:styles) ? theme.styles : nil
      @styles ||= {}
      values = variable_values(context)
      colored = color?

      build_lines.map do |number, suffix|
        if colored
          render_colored_line(number, suffix, values, context)
        else
          render_plain_line(number, suffix, values, context)
        end
      end.join("\n")
    end

    private

    def build_lines
      lines = @line_defs.each_with_index.map do |definition, index|
        [index + 1, value(definition, "suffix", "").to_s]
      end
      known = lines.map(&:first)

      @segments.map { |segment| segment_line(segment) }.uniq.sort.each do |number|
        lines << [number, ""] unless known.include?(number)
      end

      lines
    end

    def render_plain_line(number, suffix, values, context)
      segments = visible_segments(number, context)

      segments.map { |segment| plain_segment(segment, values) }.join("  ") + suffix
    end

    def render_colored_line(number, suffix, values, context)
      segments = visible_segments(number, context)
      return "" if segments.empty? && suffix.empty?
      return colored_suffix(suffix) if segments.empty?

      out = +""
      out << leading_fade(segments.first) if @leading == "square_fade"
      segments.each_with_index do |segment, index|
        out << colored_segment(segment, values)
        following = segments[index + 1]
        out << separator(segment, following) if following
      end
      out << trailing_fade(segments.last) if @trailing == "point_fade"
      out << RESET
      out << colored_suffix(suffix) unless suffix.empty?
      out
    end

    def visible_segments(number, context)
      @segments.select do |segment|
        segment_line(segment) == number && visible?(segment, context)
      end
    end

    def plain_segment(segment, values)
      value(segment, "icon", "").to_s + substitute(value(segment, "text", "").to_s, values)
    end

    def colored_segment(segment, values)
      body = (" " * @padding) + plain_segment(segment, values) + (" " * @padding)
      paint(body, fg: value(segment, "fg", nil), bg: value(segment, "bg", nil))
    end

    def leading_fade(segment)
      mixed = fade_color(segment, LEADING_MIX)
      return paint(SQUARE_FADE, fg: "night", bg: value(segment, "bg", nil)) unless mixed

      paint(" ", bg: mixed)
    end

    def trailing_fade(segment)
      mixed = fade_color(segment, TRAILING_MIX)
      return paint(ARROW, fg: value(segment, "bg", nil), bg: "night") unless mixed

      paint(" ", bg: mixed) + RESET + paint(ARROW, fg: mixed)
    end

    def separator(previous, following)
      case @separator
      when "none" then ""
      when "gradient" then gradient_separator(previous, following)
      when "fire" then fire_separator(previous, following)
      else arrow_separator(previous, following)
      end
    end

    def arrow_separator(previous, following, foreground: nil)
      paint(ARROW, fg: foreground || value(previous, "bg", nil), bg: value(following, "bg", nil))
    end

    def gradient_separator(previous, following)
      previous_bg = value(previous, "bg", nil)
      following_bg = value(following, "bg", nil)
      if previous_bg == following_bg
        return arrow_separator(previous, following, foreground: "ink")
      end

      start = truecolor_components(style_value(previous_bg))
      finish = truecolor_components(style_value(following_bg))
      return arrow_separator(previous, following) unless start && finish

      first = blend(start, finish, 0.33)
      second = blend(start, finish, 0.66)
      edge = blend(start, finish, 0.9)
      paint(" ", bg: first) +
        paint(" ", bg: second) +
        paint(ARROW, fg: edge, bg: following_bg)
    end

    def blend(first, second, ratio)
      first.zip(second).map { |a, b| (a + ((b - a) * ratio)).round }.join(";")
    end

    def fire_separator(previous, following)
      paint(" ", fg: value(previous, "bg", nil), bg: "fire1") +
        paint(ARROW, fg: "fire1", bg: "fire2") +
        paint(" ", fg: "fire2", bg: "fire3")
    end

    def colored_suffix(suffix)
      "#{paint(suffix, fg: "prompt_fg")}#{RESET}"
    end

    def paint(text, fg: nil, bg: nil)
      codes = []
      codes << style_code(fg, background: false) if fg
      codes << style_code(bg, background: true) if bg
      codes.compact!
      return text if codes.empty?

      "\e[#{codes.join(";")}m#{text}"
    end

    def style_code(name, background:)
      style = style_value(name)
      return nil if style.nil? || style.empty?

      if (rgb = rgb_components(style))
        "#{background ? "48" : "38"};2;#{rgb.join(";")}"
      else
        color_role(style, background: background)
      end
    end

    def style_value(name)
      key = name.to_s
      return @styles[key.to_sym] if @styles.key?(key.to_sym)
      return @styles[key] if @styles.key?(key)
      return FALLBACK_STYLES[key] if FALLBACK_STYLES.key?(key)
      return key if key.match?(/\A\d+(;\d+)*\z/)

      nil
    end

    def color_role(style, background:)
      params = style.split(";").map(&:strip)
      source, target = background ? %w[38 48] : %w[48 38]
      index = params.index(source)
      params[index] = target if index
      params.join(";")
    end

    def rgb_components(style)
      params = style.to_s.split(";").map(&:strip)
      return nil unless params.length == 3
      return nil unless params.all? { |part| part.match?(/\A\d{1,3}\z/) && part.to_i <= 255 }
      return nil if %w[38 48].include?(params[0]) && %w[2 5].include?(params[1])

      params.map(&:to_i)
    end

    def truecolor_components(style)
      params = style.to_s.split(";").map(&:strip)
      if params.length == 5 && %w[38 48].include?(params[0]) && params[1] == "2"
        return params[2, 3].map(&:to_i)
      end

      rgb_components(style)
    end

    def fade_color(segment, ratio)
      base = truecolor_components(style_value(value(segment, "bg", nil)))
      return nil unless base

      night = truecolor_components(style_value("night")) || [24, 24, 32]
      base.zip(night).map { |from, to| (from + ((to - from) * ratio)).round }.join(";")
    end

    def color?
      return false if @theme.respond_to?(:disabled?) && @theme.disabled?

      [ENV["NO_COLOR"], ENV["SHARKY_NO_COLOR"]].none? { |value| value && !value.empty? }
    end

    def visible?(segment, context)
      case value(segment, "when", "always").to_s
      when "always" then true
      when "git" then !context.git_branch.to_s.empty?
      when "ssh" then ssh?(context)
      when "jobs" then context.jobs_count.to_i.positive?
      when "error" then context.status.to_i != 0
      when "success" then context.status.to_i.zero?
      when "dirty" then context.git_dirty ? true : false
      when "ahead" then context.git_ahead.to_i.positive?
      when "behind" then context.git_behind.to_i.positive?
      when "staged" then context.git_staged.to_i.positive?
      when "modified" then context.git_modified.to_i.positive?
      when "untracked" then context.git_untracked.to_i.positive?
      else false
      end
    end

    def ssh?(context)
      context.respond_to?(:ssh?) ? context.ssh? : context.ssh
    end

    def variable_values(context)
      cwd = File.expand_path(context.cwd.to_s)
      home = context.home.to_s.empty? ? nil : File.expand_path(context.home.to_s)
      branch = context.git_branch.to_s
      ahead = context.git_ahead.to_i
      behind = context.git_behind.to_i

      {
        "os" => context.os.to_s,
        "arch" => context.arch.to_s,
        "user" => context.user.to_s,
        "host" => context.host.to_s,
        "cwd" => shorten_cwd(cwd, home),
        "cwd_full" => cwd,
        "cwd_base" => File.basename(cwd),
        "branch" => branch,
        "ahead" => ahead.to_s,
        "behind" => behind.to_s,
        "staged" => context.git_staged.to_i.to_s,
        "modified" => context.git_modified.to_i.to_s,
        "untracked" => context.git_untracked.to_i.to_s,
        "git" => git_summary(branch, ahead, behind),
        "status" => context.status.to_i.to_s,
        "duration" => format("%.2fs", context.duration_ms.to_f / 1000),
        "jobs" => context.jobs_count.to_i.to_s,
        "ssh" => ssh?(context) ? "1" : "0",
        "dirty" => context.git_dirty ? "1" : "0"
      }
    end

    def shorten_cwd(cwd, home)
      shortened = if home && (cwd == home || cwd.start_with?("#{home}/"))
                    "~#{cwd[home.length..]}"
                  else
                    cwd
                  end

      return shortened if shortened.length <= CWD_MAX

      base = File.basename(cwd)
      shortened.start_with?("~") ? "~/…/#{base}" : "…/#{base}"
    end

    def git_summary(branch, ahead, behind)
      return "" if branch.empty?

      parts = [branch, "•"]
      parts << "↑#{ahead}" if ahead.positive?
      parts << "↓#{behind}" if behind.positive?
      parts.length == 2 ? branch : parts.join(" ")
    end

    def substitute(template, values)
      template.gsub(/\{\{|\}\}|\{([^{}]*)\}/) do |token|
        case token
        when "{{" then "{"
        when "}}" then "}"
        else values.fetch(token[1...-1], "")
        end
      end
    end

    def padding_value
      number = Integer(value(@prompt, "padding", 1))
      number.negative? ? 0 : number
    rescue ArgumentError, TypeError
      1
    end

    def segment_line(segment)
      number = Integer(value(segment, "line", 1))
      number.positive? ? number : 1
    rescue ArgumentError, TypeError
      1
    end

    def table(data, key)
      entry = value(data, key, {})
      entry.is_a?(Hash) ? entry : {}
    end

    def array(data, key)
      entry = value(data, key, [])
      entry.is_a?(Array) ? entry : []
    end

    def value(hash, key, default)
      return default unless hash.is_a?(Hash)
      return hash[key] if hash.key?(key)
      return hash[key.to_sym] if hash.key?(key.to_sym)

      default
    end
  end
end
