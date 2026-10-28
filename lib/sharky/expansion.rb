# frozen_string_literal: true

require_relative "arithmetic"

module Sharky
  class Expansion
    Chunk = Struct.new(:text, :split, :glob, :quoted_empty, keyword_init: true)
    IFS = " \t\n"
    GLOB_MAGIC = "*?["
    ParameterError = Class.new(StandardError)

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
      when :parameter
        evaluate_parameter(part.value, quoted: quoted)
      when :arithmetic
        text = Arithmetic.evaluate(part.value.source, env: @env).to_s
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

    def evaluate_parameter(parameter, quoted:)
      value = parameter_value(parameter.name)
      text =
        case parameter.operator
        when nil
          value
        when :length
          value.length.to_s
        when :default
          value.empty? ? expand_argument(parameter.argument) : value
        when :assign
          if value.empty?
            assigned = expand_argument(parameter.argument)
            @env[parameter.name] = assigned
            assigned
          else
            value
          end
        when :error
          if value.empty?
            message = parameter.argument.to_s
            message = "parameter null or not set" if message.empty?
            raise ParameterError, "#{parameter.name}: #{message}"
          end
          value
        when :alternate
          value.empty? ? "" : expand_argument(parameter.argument)
        when :trim_prefix_short
          trim_prefix(value, parameter.argument, longest: false)
        when :trim_prefix_long
          trim_prefix(value, parameter.argument, longest: true)
        when :trim_suffix_short
          trim_suffix(value, parameter.argument, longest: false)
        when :trim_suffix_long
          trim_suffix(value, parameter.argument, longest: true)
        when :substitute
          substitute(value, parameter.argument, all: false)
        when :substitute_all
          substitute(value, parameter.argument, all: true)
        else
          value
        end

      [chunk(text, split: !quoted, glob: !quoted, quoted_empty: quoted && text.empty?)]
    end

    def expand_argument(raw)
      return "" if raw.nil?

      words = Lexer.new(raw).tokenize.select { |token| token.type == :word }.map do |token|
        AST::Word.new(parts: token.value.parts, pos: token.pos)
      end
      return raw if words.empty?

      words.map { |word| expand_word(word, quoted: true).join }.join(" ")
    rescue LexError
      raw
    end

    def trim_prefix(value, pattern, longest:)
      indexes = (0..value.length).select do |index|
        File.fnmatch(pattern.to_s, value[0...index], File::FNM_DOTMATCH)
      end
      index = longest ? indexes.max : indexes.min
      index ? value[index..] : value
    end

    def trim_suffix(value, pattern, longest:)
      indexes = (0..value.length).select do |index|
        File.fnmatch(pattern.to_s, value[index..], File::FNM_DOTMATCH)
      end
      index = longest ? indexes.min : indexes.max
      index ? value[0...index] : value
    end

    def substitute(value, argument, all:)
      pattern, replacement = argument.to_s.split("/", 2)
      return value if pattern.nil? || pattern.empty?

      all ? value.gsub(pattern, replacement.to_s) : value.sub(pattern, replacement.to_s)
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
