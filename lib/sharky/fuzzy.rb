# frozen_string_literal: true

require "find"

module Sharky
  module Fuzzy
    FILE_LIMIT = 20_000
    BASE = 2
    BOUNDARY = 6
    CONSECUTIVE = 8
    PREFIX = 4
    EXACT = 8
    GAP = 1
    BOUNDARY_CHARS = "/ ._-"

    module_function

    def score(query, candidate)
      query = query.to_s.downcase
      text = candidate.to_s.downcase
      return 0 if query.empty? || text.empty? || query.length > text.length

      characters = text.chars
      previous = Array.new(characters.length)

      query.each_char.with_index do |needle, index|
        current = Array.new(characters.length)
        best = nil

        characters.each_with_index do |character, position|
          if index.zero?
            next unless character == needle

            current[position] = character_score(characters, position) - (position * GAP)
          else
            if position.positive? && previous[position - 1]
              value = previous[position - 1] + ((position - 1) * GAP)
              best = value if best.nil? || value > best
            end
            next unless character == needle

            from_before = best && best - ((position - 1) * GAP)
            if previous[position - 1]
              consecutive = previous[position - 1] + CONSECUTIVE
              from_before = consecutive if from_before.nil? || consecutive > from_before
            end
            current[position] = character_score(characters, position) + from_before if from_before
          end
        end

        return 0 unless current.any?

        previous = current
      end

      result = previous.compact.max
      return 0 unless result

      result += EXACT if text == query
      [result, 1].max
    end

    def rank(query, candidates)
      candidates.each_with_index.filter_map do |candidate, index|
        value = score(query, candidate)
        [candidate, value, index] if value.positive?
      end.sort_by { |candidate, value, index| [-value, index] }.map(&:first)
    end

    def files(root, limit: FILE_LIMIT)
      root = File.expand_path(root)
      return [[], false] unless File.directory?(root)

      prefix = root.end_with?(File::SEPARATOR) ? root : root + File::SEPARATOR
      results = []
      capped = false

      Find.find(root) do |path|
        next if path == root

        name = File.basename(path)
        if name == ".git" || (File.directory?(path) && name.start_with?("."))
          Find.prune if File.directory?(path)
          next
        end

        relative = path.delete_prefix(prefix)
        results << (File.directory?(path) ? relative + File::SEPARATOR : relative)

        if results.length >= limit
          capped = true
          break
        end
      end

      [results.sort, capped]
    rescue SystemCallError
      [results.sort, capped]
    end

    def character_score(characters, position)
      bonus = BASE
      bonus += BOUNDARY if position.zero?
      bonus += PREFIX if position.zero?
      if position.positive? && BOUNDARY_CHARS.include?(characters[position - 1])
        bonus += BOUNDARY
      end
      bonus
    end
    private_class_method :character_score
  end
end
