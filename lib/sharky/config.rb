# frozen_string_literal: true

require_relative "api"

module Sharky
  class Config
    CONFIG_FILE = "sharky.rb"
    LEGACY_CONFIG_FILE = "init.rb"

    attr_reader :config_dir, :data_dir, :cache_dir

    def initialize(dir: nil)
      home = ENV["HOME"] || Dir.home
      @config_dir = File.expand_path(dir || ENV["SHARKY_CONFIG"] || File.join(ENV["XDG_CONFIG_HOME"] || File.join(home, ".config"), "sharky"))
      @data_dir = File.expand_path(File.join(ENV["XDG_DATA_HOME"] || File.join(home, ".local", "share"), "sharky"))
      @cache_dir = File.expand_path(File.join(ENV["XDG_CACHE_HOME"] || File.join(home, ".cache"), "sharky"))
    end

    def ensure_layout!
      config = File.join(@config_dir, CONFIG_FILE)
      legacy = File.join(@config_dir, LEGACY_CONFIG_FILE)
      return false if File.exist?(config) || File.exist?(legacy)

      require "fileutils"
      FileUtils.mkdir_p(@config_dir)
      File.open(config, File::WRONLY | File::CREAT | File::EXCL, 0o644) {}
      true
    rescue SystemCallError
      false
    end

    def load_into(runtime)
      Sharky.session = Session.from_runtime(runtime)
      runtime.config_dir = @config_dir

      config = File.join(@config_dir, CONFIG_FILE)
      legacy = File.join(@config_dir, LEGACY_CONFIG_FILE)
      path = if File.file?(config)
               config
             elsif File.file?(legacy)
               legacy
             end
      return [] unless path

      warnings = []
      if path == legacy
        warnings << "sharky: legacy config #{legacy} is deprecated; rename it to #{CONFIG_FILE}"
      elsif File.file?(legacy)
        warnings << "sharky: ignoring legacy config #{legacy}; using #{CONFIG_FILE}"
      end

      begin
        load(path)
        warnings
      rescue StandardError, ScriptError => error
        warnings + ["sharky: config error in #{path}: #{error.message}"]
      end
    end
  end
end
