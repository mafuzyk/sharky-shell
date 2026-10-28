# frozen_string_literal: true

require "fileutils"
require "json"

module Sharky
  class Z
    MAX_ENTRIES = 20_000
    COMPACT_LIMIT = 10_000
    HALF_LIFE = 30 * 24 * 60 * 60

    attr_reader :path

    def self.default_path(env = ENV, fallback: true)
      override = env["SHARKY_Z"]
      return File.expand_path(override) if override && !override.empty?

      data_home = env["XDG_DATA_HOME"]
      if data_home.nil? || data_home.empty?
        home = env["HOME"]
        home = Dir.home if (home.nil? || home.empty?) && fallback
        return nil if home.nil? || home.empty?

        data_home = File.join(home, ".local", "share")
      end
      File.join(data_home, "sharky", "z.jsonl")
    end

    def initialize(path: nil)
      path ||= self.class.default_path
      raise ArgumentError, "z data path is not configured" if path.nil? || path.empty?

      @path = File.expand_path(path)
      @entries = nil
      @loaded = false
    end

    def record(path, time: Time.now)
      path = File.expand_path(path)
      stamp = time.to_i
      load_entries
      ensure_directory
      entry = { t: stamp, p: path }
      append_entry(entry)
      @entries << entry
      compact! if @entries.length > MAX_ENTRIES
      self
    end

    def query(tokens)
      matches(tokens).first
    end

    def matches(tokens)
      load_entries
      tokens = normalize_tokens(tokens)
      now = Time.now.to_f
      grouped = @entries.group_by { |entry| entry[:p] }
      ranked = grouped.filter_map do |candidate, visits|
        next unless tokens.all? { |token| candidate.downcase.include?(token) }

        latest = visits.map { |entry| entry[:t] }.max
        [candidate, path_score(candidate, visits, tokens, now), latest]
      end
      ranked.sort_by { |candidate, score, latest| [-score, -latest, candidate] }.map(&:first)
    end

    def compact!(limit: COMPACT_LIMIT)
      limit = Integer(limit)
      ensure_directory
      kept = []
      File.open(@path, File::RDWR | File::CREAT, 0600) do |lock|
        lock.chmod(0600)
        lock.flock(File::LOCK_EX)
        begin
          lock.rewind
          entries = parse_entries(lock.read)
          kept = compact_entries(entries, limit)
          write_entries(kept)
        ensure
          lock.flock(File::LOCK_UN)
        end
      end
      @entries = kept
      @loaded = true
      self
    end

    private

    def load_entries
      return if @loaded

      @entries = File.exist?(@path) ? parse_entries(File.read(@path)) : []
      @loaded = true
    end

    def append_entry(entry)
      loop do
        file = File.open(@path, File::WRONLY | File::CREAT | File::APPEND, 0600)
        begin
          file.flock(File::LOCK_EX)
          current = File.stat(@path)
          own = file.stat
          next unless current.ino == own.ino && current.dev == own.dev

          file.chmod(0600)
          file.write(JSON.generate("t" => entry[:t], "p" => entry[:p]) + "\n")
          file.flush
          return
        ensure
          file.close
        end
      end
    end

    def parse_entries(content)
      content.each_line.filter_map do |line|
        data = JSON.parse(line)
        next unless data.is_a?(Hash)

        stamp = data["t"]
        candidate = data["p"]
        next unless stamp.is_a?(Numeric) && candidate.is_a?(String) && !candidate.empty?

        { t: stamp.to_i, p: candidate }
      rescue JSON::ParserError
        nil
      end
    end

    def normalize_tokens(tokens)
      Array(tokens).flat_map { |token| token.to_s.split(/\s+/) }.map(&:downcase).reject(&:empty?)
    end

    def path_score(path, visits, tokens, now)
      recency = visits.sum do |entry|
        age = [now - entry[:t], 0].max
        0.5**(age / HALF_LIFE)
      end
      recency + component_bonus(path, tokens)
    end

    def component_bonus(path, tokens)
      components = path.downcase.split(File::SEPARATOR).reject(&:empty?)
      position = 0
      bonus = 0
      tokens.each do |token|
        while position < components.length
          component = components[position]
          position += 1
          if component == token
            bonus += 8
            break
          elsif component.start_with?(token)
            bonus += 4
            break
          elsif component.include?(token)
            bonus += 1
            break
          end
        end
      end
      bonus
    end

    def compact_entries(entries, limit)
      return [] if limit <= 0 || entries.empty?

      now = Time.now.to_f
      grouped = entries.group_by { |entry| entry[:p] }
      scores = grouped.to_h do |candidate, visits|
        [candidate, path_score(candidate, visits, [], now)]
      end
      ranked = grouped.keys.sort_by do |candidate|
        [-scores[candidate], candidate]
      end
      kept = []
      ranked.each do |candidate|
        break if kept.length >= limit

        kept << grouped[candidate].max_by { |entry| entry[:t] }
      end

      if kept.length < limit
        retained = kept.each_with_object({}) { |entry, ids| ids[entry.object_id] = true }
        remaining = entries.reject { |entry| retained[entry.object_id] }
        remaining = remaining.sort_by do |entry|
          [-scores[entry[:p]], -entry[:t], entry[:p]]
        end
        kept.concat(remaining.first(limit - kept.length))
      end

      kept.sort_by { |entry| [entry[:t], entry[:p]] }
    end

    def write_entries(entries)
      directory = File.dirname(@path)
      temporary = File.join(directory, ".#{File.basename(@path)}.tmp.#{Process.pid}.#{rand(1_000_000)}")
      File.open(temporary, File::WRONLY | File::CREAT | File::EXCL, 0600) do |file|
        file.chmod(0600)
        entries.each do |entry|
          file.write(JSON.generate("t" => entry[:t], "p" => entry[:p]) + "\n")
        end
        file.flush
        file.fsync
      end
      File.rename(temporary, @path)
    ensure
      File.unlink(temporary) if temporary && File.exist?(temporary)
    end

    def ensure_directory
      directory = File.dirname(@path)
      existed = File.directory?(directory)
      FileUtils.mkdir_p(directory, mode: 0700)
      return if existed && File.basename(directory) != "sharky"

      File.chmod(0700, directory)
    end
  end
end
