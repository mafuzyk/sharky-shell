# frozen_string_literal: true

require_relative "arithmetic"

module Sharky
  class Expansion
    Chunk = Struct.new(:text, :split, :glob, :quoted_empty, keyword_init: true)
    IFS = " \t\n"
    GLOB_MAGIC = "*?["
    ParameterError = Class.new(StandardError)

    def initialize(env: {}, positional: [], script_name: "sharky", nounset: false, arrays: {}, cwd: Dir.pwd, command_substitution: nil)
      @env = env
      @positional = Array(positional).map(&:to_s)
      @script_name = script_name.to_s
      @nounset = nounset
      @arrays = arrays
      @cwd = cwd
      @command_substitution = command_substitution
    end

    def expand_word(word, quoted: false)
      brace_expand(word, quoted: quoted).flat_map do |variant|
        fields = split_fields(variant.parts.flat_map { |part| expand_part(part, quoted: quoted) })
        fields.flat_map { |field| glob_field(field) }
      end
    end

    def expand_words(words)
      words.flat_map { |word| expand_word(word) }
    end

    def expand_assignment(word)
      word.parts.flat_map { |part| expand_part(part, quoted: true) }.map(&:text).join
    end

    private

    def brace_expand(word, quoted:)
      return [word] if quoted || word.parts.length != 1
      part = word.parts.first
      return [word] unless part.type == :literal && part.value.include?("{")

      brace_expand_text(part.value).map do |text|
        AST::Word.new(
          parts: [AST::Part.new(type: :literal, value: text, pos: part.pos)],
          pos: word.pos
        )
      end
    end

    def brace_expand_text(text)
      open = text.index("{")
      return [text] unless open

      close = matching_brace(text, open)
      return [text] unless close

      prefix = text[0...open]
      suffix = text[(close + 1)..]
      body = text[(open + 1)...close]
      options = brace_options(body)
      return [text] if options.nil? || options.empty?

      expansions = options.flat_map { |option| brace_expand_text(prefix + option + suffix) }
      expansions.empty? ? [text] : expansions
    end

    def matching_brace(text, open)
      depth = 0
      index = open

      while index < text.length
        case text[index]
        when "{"
          depth += 1
        when "}"
          depth -= 1
          return index if depth.zero?
        end
        index += 1
      end

      nil
    end

    def brace_options(body)
      if (range = /\A(-?\d+)\.\.(-?\d+)(?:\.\.(-?\d+))?\z/.match(body))
        return numeric_range(range)
      end
      if (range = /\A(.)\.\.(.)(?:\.\.(-?\d+))?\z/.match(body))
        return character_range(range)
      end

      parts = []
      depth = 0
      start = 0
      body.each_char.with_index do |char, index|
        case char
        when "{"
          depth += 1
        when "}"
          depth -= 1
        when ","
          next unless depth.zero?

          parts << body[start...index]
          start = index + 1
        end
      end
      return nil if parts.empty?

      parts << body[start..]
      parts
    end

    def numeric_range(match)
      first = Integer(match[1], 10)
      last = Integer(match[2], 10)
      step = match[3] ? Integer(match[3], 10) : (first <= last ? 1 : -1)
      return nil if step.zero?

      step = -step.abs if first > last
      values = []
      value = first
      while first <= last ? value <= last : value >= last
        values << value.to_s
        value += step
        break if values.length > 10_000
      end
      values.empty? ? nil : values
    end

    def character_range(match)
      first = match[1].ord
      last = match[2].ord
      step = match[3] ? Integer(match[3], 10) : (first <= last ? 1 : -1)
      return nil if step.zero?

      step = -step.abs if first > last
      values = []
      value = first
      while first <= last ? value <= last : value >= last
        values << value.chr
        value += step
        break if values.length > 10_000
      end
      values.empty? ? nil : values
    end

    def expand_part(part, quoted:)
      case part.type
      when :literal
        [chunk(part.value, split: false, glob: !quoted)]
      when :single, :escaped
        [chunk(part.value, split: false, glob: false, quoted_empty: part.value.empty?)]
      when :double
        inner = part.value.flat_map { |child| expand_part(child, quoted: true) }
        text = inner.map(&:text).join
        [chunk(text, split: false, glob: false, quoted_empty: text.empty?)]
      when :variable, :special
        text = parameter_value(part.value)
        [chunk(text, split: !quoted, glob: !quoted, quoted_empty: quoted && text.empty?)]
      when :positional
        text = positional_value(part.value)
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

    def parameter_value(name, nounset: @nounset, index: nil)
      if @arrays.key?(name)
        array = @arrays[name]
        return array.join(" ") if ["@", "*"].include?(index)
        return array[index.to_i].to_s if index

        return array.first.to_s
      end

      value =
        case name
        when "?" then @env.fetch("?", "0")
        when "$" then @env.fetch("$", Process.pid.to_s)
        when "!" then @env.fetch("!", "")
        when "#" then @positional.length.to_s
        when "@", "*" then @positional.join(" ")
        when /\A\d+\z/
          index = name.to_i - 1
          if nounset && index >= @positional.length
            raise ParameterError, "#{name}: unbound variable"
          end
          @positional[index].to_s
        else
          raise ParameterError, "#{name}: unbound variable" if nounset && !@env.key?(name)

          @env.fetch(name, "")
        end
      value.to_s
    end

    def positional_value(name)
      case name
      when "#" then @positional.length.to_s
      when "@", "*" then @positional.join(" ")
      when "0" then @script_name
      when /\A\d+\z/
        index = name.to_i - 1
        raise ParameterError, "#{name}: unbound variable" if @nounset && index >= @positional.length

        @positional[index].to_s
      else ""
      end
    end

    def evaluate_parameter(parameter, quoted:)
      no_error = [:default, :assign, :alternate, :error].include?(parameter.operator)
      if parameter.operator == :length && parameter.index && @arrays.key?(parameter.name)
        text = @arrays[parameter.name].length.to_s
        return [chunk(text, split: !quoted, glob: !quoted, quoted_empty: quoted && text.empty?)]
      end

      value = parameter_value(
        parameter.name,
        nounset: @nounset && !no_error,
        index: parameter.index
      )
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
