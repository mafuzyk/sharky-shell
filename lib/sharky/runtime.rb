# frozen_string_literal: true

module Sharky
  class Runtime
    attr_accessor :prompt, :starship, :theme, :theme_layout, :config_dir, :shell
    attr_reader :aliases, :abbreviations, :bindings, :commands, :completions, :cache_generation

    def initialize
      @prompt = nil
      @starship = nil
      @theme = {}
      @theme_layout = nil
      @config_dir = nil
      @shell = nil
      @aliases = {}
      @abbreviations = {}
      @bindings = {}
      @commands = {}
      @completions = {}
      @cache_generation = 0
    end

    def reload_config
      @aliases = {}
      @abbreviations = {}
      @bindings = {}
      @completions = {}
      @prompt = nil
      @starship = nil
      @theme = {}
      @theme_layout = nil
      @cache_generation += 1
      return [] unless @config_dir

      Config.new(dir: @config_dir).load_into(self)
    end

    def self.control_operator?(value)
      value.match?(/[|&;<>]/) || value.include?("\n")
    end
  end
end
