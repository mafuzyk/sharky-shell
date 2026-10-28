# frozen_string_literal: true

module Sharky
  class Completion
    Candidate = Struct.new(:value, :description, keyword_init: true)

    ASSIGNMENT = /\A[A-Za-z_][A-Za-z0-9_]*=/
    TOKEN_BREAK = /[\s|&;<>()]/
    VARIABLE = /\$\{?([A-Za-z_][A-Za-z0-9_]*)?\z/
    SHELL_KEYWORDS = %w[
      if then elif else fi for in do done while until case esac function
      break continue return
    ].freeze

    def initialize(runtime:, shell:)
      @runtime = runtime
      @shell = shell
      @path_key = nil
      @path_commands = []
    end

    def token(line, cursor)
      prefix = line.to_s[0...cursor.to_i.clamp(0, line.to_s.length)]
      if (match = prefix.match(VARIABLE))
        start = match.begin(1) || prefix.length
        return [start, prefix[start..].to_s]
      end

      start = prefix.rindex(TOKEN_BREAK)
      start = start ? start + 1 : 0
      [start, prefix[start..].to_s]
    end

    def candidates(line, cursor)
      line = line.to_s
      cursor = cursor.to_i.clamp(0, line.length)
      start, word = token(line, cursor)
      before = line[0...start]

      if variable_token?(before)
        names = (@shell.env.keys + @shell.arrays.keys).grep(/\A[A-Za-z_][A-Za-z0-9_]*\z/).uniq.sort
        return matched(names, word, "variable")
      end

      command = command_name(before)
      custom = command ? user_candidates(command, word, line, cursor) : []

      if command_position?(before)
        values = command_candidates(word)
        values.concat(file_candidates(word)) if word.match?(%r{\A[.~/]|/})
        return unique(custom + values)
      end

      if git_branch_position?(before, word)
        return unique(custom + branch_candidates(word) + file_candidates(word))
      end

      unique(custom + file_candidates(word))
    end

    private

    def candidate(value, description = nil)
      Candidate.new(value: value, description: description)
    end

    def matched(values, prefix, description)
      values.select { |value| value.start_with?(prefix) }
            .map { |value| candidate(value, description) }
    end

    def command_candidates(word)
      matched(SHELL_KEYWORDS, word, "keyword") +
        matched(@runtime.aliases.keys, word, "alias") +
        matched(@runtime.abbreviations.keys, word, "abbreviation") +
        matched(@runtime.commands.keys, word, "function") +
        matched(@shell.functions.keys, word, "function") +
        matched(Builtins::TABLE.keys, word, "builtin") +
        matched(path_commands, word, "executable")
    end

    def path_commands
      path = @shell.env["PATH"].to_s
      cache_key = [@runtime.cache_generation, path]
      return [] if path.empty?
      return @path_commands if @path_key == cache_key

      @path_key = cache_key
      @path_commands = path.split(File::PATH_SEPARATOR).flat_map do |directory|
        next [] if directory.empty?

        Dir.children(directory).select do |entry|
          File.executable?(File.join(directory, entry)) &&
            !File.directory?(File.join(directory, entry))
        end
      rescue SystemCallError
        []
      end.uniq.sort
    end

    def file_candidates(word)
      directory, prefix, output_prefix = split_path(word)
      expanded = expand_directory(directory)
      entries = Dir.children(expanded).sort
      entries.filter_map do |entry|
        next if entry.start_with?(".") && !prefix.start_with?(".")

        path = File.join(expanded, entry)
        next unless entry.start_with?(prefix)

        suffix = File.directory?(path) ? File::SEPARATOR : ""
        candidate(output_prefix + entry + suffix, "path")
      end
    rescue SystemCallError
      []
    end

    def split_path(word)
      if word.end_with?(File::SEPARATOR)
        directory = word.chomp(File::SEPARATOR)
        [directory, "", word]
      else
        directory = File.dirname(word)
        prefix = File.basename(word)
        output = directory == "." ? "" : directory + File::SEPARATOR
        [directory, prefix, output]
      end
    end

    def expand_directory(directory)
      home = @shell.env["HOME"] || Dir.home
      if directory == "~"
        home
      elsif directory.start_with?("~/")
        File.join(home, directory[2..].to_s)
      else
        File.expand_path(directory, @shell.cwd)
      end
    end

    def command_position?(before)
      segment = before.split(/[|&;()\n]/).last.to_s
      return false if segment.match?(/[<>]\s*\z/)

      words = segment.scan(/[^\s<>]+/)
      words.empty? || words.all? { |word| word.match?(ASSIGNMENT) }
    end

    def command_name(before)
      segment = before.split(/[|&;()\n]/).last.to_s
      segment.scan(/[^\s<>]+/).find { |word| !word.match?(ASSIGNMENT) }
    end

    def git_branch_position?(before, word)
      return false if word.start_with?("-")

      words = before.split(/[|&;()\n]/).last.to_s.scan(/[^\s<>]+/)
      return false unless words.first == "git"
      return false if words.include?("--")

      case words[1]
      when "checkout", "switch", "merge", "rebase"
        words.length >= 2
      when "branch"
        %w[-d -D --delete].include?(words[2])
      else
        false
      end
    end

    def branch_candidates(prefix)
      output = IO.popen(
        ["git", "-C", @shell.cwd, "for-each-ref", "--format=%(refname:short)", "refs/heads"],
        err: File::NULL, &:read
      )
      output.lines.map(&:strip)
            .select { |branch| branch.start_with?(prefix) }
            .map { |branch| candidate(branch, "branch") }
    rescue SystemCallError
      []
    end

    def user_candidates(command, word, line, cursor)
      completion = @runtime.completions[command]
      return [] unless completion

      Array(completion.call(word, line, cursor)).filter_map do |item|
        case item
        when Candidate
          item
        when String
          candidate(item)
        when Array
          candidate(item[0].to_s, item[1]&.to_s)
        end
      end
    rescue StandardError
      []
    end

    def variable_token?(before)
      before.end_with?("$") || before.end_with?("${")
    end

    def unique(candidates)
      candidates.each_with_object({}) do |item, result|
        result[item.value] ||= item
      end.values
    end
  end
end
