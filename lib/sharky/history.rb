# frozen_string_literal: true

require "fileutils"

module Sharky
  class History
    MAX_ENTRIES = 10_000

    attr_reader :path, :entries

    def self.default_path
      override = ENV["SHARKY_HISTORY"]
      return File.expand_path(override) if override && !override.empty?

      data_home = ENV["XDG_DATA_HOME"]
      data_home = File.join(Dir.home, ".local", "share") if data_home.nil? || data_home.empty?
      File.join(data_home, "sharky", "history")
    end

    def initialize(path: nil)
      @path = File.expand_path(path || self.class.default_path)
      @entries = []
      @saved_count = 0
    end

    def load
      @entries = if File.exist?(@path)
                   File.readlines(@path, chomp: true).map { |line| unescape(line) }.reject(&:empty?)
                 else
                   []
                 end
      trim
      @saved_count = @entries.length
      self
    end

    def add(command)
      command = command.to_s
      return self if command.empty?
      return self if @entries.include?(command)

      @entries << command
      trim
      self
    end

    def save
      ensure_directory
      if @saved_count >= @entries.length
        if File.exist?(@path)
          File.chmod(0600, @path)
        else
          create_empty_file
        end
        return self
      end

      File.open(@path, File::WRONLY | File::CREAT | File::APPEND, 0600) do |file|
        file.chmod(0600)
        @entries[@saved_count..].each { |entry| file.write("#{escape(entry)}\n") }
        file.flush
      end
      @saved_count = @entries.length
      self
    end

    def search(prefix)
      prefix = prefix.to_s
      @entries.reverse.select { |entry| entry.start_with?(prefix) }
    end

    def clear
      @entries.clear
      @saved_count = 0
      ensure_directory
      File.open(@path, File::WRONLY | File::CREAT | File::TRUNC, 0600) do |file|
        file.chmod(0600)
      end
      self
    end

    private

    def trim
      return if @entries.length <= MAX_ENTRIES

      removed = @entries.length - MAX_ENTRIES
      @entries.shift(removed)
      @saved_count = [@saved_count - removed, 0].max
    end

    def ensure_directory
      directory = File.dirname(@path)
      existed = File.directory?(directory)
      FileUtils.mkdir_p(directory, mode: 0700)
      return if existed && File.basename(directory) != "sharky"

      File.chmod(0700, directory)
    end

    def create_empty_file
      File.open(@path, File::WRONLY | File::CREAT, 0600) do |file|
        file.chmod(0600)
      end
    end

    def escape(entry)
      entry.gsub(/[\\\n]/) { |char| char == "\n" ? "\\n" : "\\\\" }
    end

    def unescape(line)
      line.gsub(/\\(\\|n)/) { Regexp.last_match(1) == "n" ? "\n" : "\\" }
    end
  end
end
