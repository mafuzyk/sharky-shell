# frozen_string_literal: true

module Sharky
  class Theme
    DEFAULT = {
      prompt: "1;38;5;75",
      error: "38;5;203",
      success: "38;5;114",
      directory: "1;38;5;111",
      git: "38;5;180",
      hint: "38;5;244",
      suggestion: "38;5;244",
      selection: "48;5;238"
    }.freeze

    attr_reader :styles

    def initialize(styles = {})
      @styles = DEFAULT.merge(normalize(styles))
    end

    def merge(styles)
      self.class.new(@styles.merge(normalize(styles)))
    end

    def style(name, text)
      code = @styles[name.to_sym]
      text = text.to_s
      return text if code.nil? || disabled?

      "\e[#{code}m#{text}\e[0m"
    end

    def disabled?
      [ENV["NO_COLOR"], ENV["SHARKY_NO_COLOR"]].any? { |value| value && !value.empty? }
    end

    private

    def normalize(styles)
      styles.to_h.each_with_object({}) { |(name, value), result| result[name.to_sym] = value }
    end
  end
end
