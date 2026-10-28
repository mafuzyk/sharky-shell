# frozen_string_literal: true

module Sharky
  class Expansion
    Chunk = Struct.new(:text, :split, :glob, :quoted_empty, keyword_init: true)
    IFS = " \t\n"
    GLOB_MAGIC = "*?["

    def initialize(env: {}, cwd: Dir.pwd, command_substitution: nil)
      @env = env
      @cwd = cwd
      @command_substitution = command_substitution
    end

    def expand_word(word, quoted: false)
      fields = split_fields(word.parts.flat_map { |part| expand_part(part, quoted: quoted) })
      fields.flat_map { |field| glob_field(field) }
    end

    def expand_words(words)
      words.flat_map { |word| expand_word(word) }
    end

    def expand_assignment(word)
      word.parts.flat_map { |part| expand_part(part, quoted: true) }.map(&:text).join
    end

    private

    def expand_part(part, quoted:)
      case part.type
      when :literal
        [chunk(part.value, split: false, glob: !quoted)]
      when :single
        [chunk(part.value, split: false, glob: false, quoted_empty: part.value.empty?)]
      when :double
        inner = part.value.flat_map { |child| expand_part(child, quoted: true) }
        text = inner.map(&:text).join
        [chunk(text, split: false, glob: false, quoted_empty: text.empty?)]
      when :variable, :special
        text = parameter_value(part.value)
        [chunk(text, split: !quoted, glob: !quoted, quoted_empty: quoted && text.empty?)]
      when :cmdsub
        text = command_substitution(part.value)
        [chunk(text, split: !quoted, glob: !quoted, quoted_empty: quoted && text.empty?)]
      when :tilde
        text = @env["HOME"] || Dir.home
        [chunk(text, split: false, glob: false, quoted_empty: quoted && text.empty?)]
      else
        [chunk(part.value.to_s, split: false, glob: false)]
      end
    end

    def chunk(text, split:, glob:, quoted_empty: false)
      Chunk.new(text: text.to_s, split: split, glob: glob, quoted_empty: quoted_empty)
    end

    def parameter_value(name)
      value =
        case name
        when "?" then @env.fetch("?", "0")
        when "$" then @env.fetch("$", Process.pid.to_s)
        when "!" then @env.fetch("!", "")
        else @env.fetch(name, "")
        end
      value.to_s
    end

    def command_substitution(program)
      program = Parser.parse(program) if program.is_a?(String)
      return "" unless @command_substitution

      @command_substitution.call(program).to_s.sub(/\n+\z/, "")
    end

    def split_fields(chunks)
      fields = []
      current = []
      quoted_empty = false

      chunks.each do |chunk|
        quoted_empty ||= chunk.quoted_empty
        chunk.text.each_char do |char|
          if chunk.split && IFS.include?(char)
            unless current.empty?
              fields << current
              current = []
            end
          else
            current << [char, chunk.glob]
          end
        end
      end

      fields << current unless current.empty?
      return [[["", false]]] if fields.empty? && quoted_empty

      fields
    end

    def glob_field(field)
      text = field.map(&:first).join
      return [text] unless field.any? { |char, glob| glob && GLOB_MAGIC.include?(char) }

      pattern = field.map { |char, glob| glob ? char : escape_glob(char) }.join
      matches = glob_paths(pattern)
      matches.empty? ? [text] : matches
    end

    def escape_glob(char)
      "*?[]{}".include?(char) || char == "\\" ? "\\#{char}" : char
    end

    def glob_paths(pattern)
      if pattern.start_with?("/")
        Dir.glob(pattern)
      else
        Dir.chdir(@cwd) { Dir.glob(pattern) }
      end
    end
  end
end
