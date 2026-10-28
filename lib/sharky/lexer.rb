module Sharky
  Token = Struct.new(:type, :value, :pos, keyword_init: true)

  class LexError < StandardError
    attr_reader :pos

    def initialize(message, pos: nil, incomplete: false)
      super(message)
      @pos = pos
      @incomplete = incomplete
    end

    def incomplete?
      @incomplete
    end
  end

  class Lexer
    OPERATOR_CHARS = [" ", "\t", "\n", ";", "&", "|", "(", ")", "<", ">"].freeze
    NAME_START = /\A[A-Za-z_]\z/
    NAME_CHAR = /\A[A-Za-z0-9_]\z/

    def initialize(source, compound: false)
      @source = source
      # Opt-in until the parser consumes compound tokens instead of rejecting them.
      @compound = compound
      @pos = 0
      @tokens = []
    end

    def tokenize
      loop do
        skip_blanks
        char = current
        if char.nil?
          add(:eof)
          break
        end

        case char
        when "\n"
          add(:newline)
          @pos += 1
        when "#"
          skip_comment
        when ";"
          if @compound && peek == ";" && peek(2) == "&"
            add(:case_continue)
            @pos += 3
          elsif @compound && peek == "&"
            add(:case_break)
            @pos += 2
          else
            add(:semi)
            @pos += 1
          end
        when "&"
          scan_ampersand
        when "|"
          if peek == "|"
            add(:or_if)
            @pos += 2
          else
            add(:pipe)
            @pos += 1
          end
        when "("
          if @compound && peek == "("
            add(:arith_start)
            @pos += 2
          else
            add(:lparen)
            @pos += 1
          end
        when ")"
          if @compound && peek == ")"
            add(:arith_end)
            @pos += 2
          else
            add(:rparen)
            @pos += 1
          end
        when "!"
          if @compound && bang_delimiter?
            add(:bang)
            @pos += 1
          else
            add_word
          end
        when "{"
          if @compound && group_delimiter?
            add(:lbrace)
            @pos += 1
          else
            add_word
          end
        when "}"
          if @compound && group_delimiter?
            add(:rbrace)
            @pos += 1
          else
            add_word
          end
        when "["
          if @compound && peek == "["
            add(:test_start)
            @pos += 2
          else
            add_word
          end
        when "]"
          if @compound && peek == "]"
            add(:test_end)
            @pos += 2
          else
            add_word
          end
        when ">"
          scan_output_redirect(nil)
        when "<"
          scan_input_redirect(nil)
        else
          if digit?(char)
            scan_number_redirect
          else
            add_word
          end
        end
      end
      @tokens
    end

    private

    def current
      @source[@pos]
    end

    def peek(offset = 1)
      @source[@pos + offset]
    end

    def digit?(char)
      char && char >= "0" && char <= "9"
    end

    def operator_char?(char)
      OPERATOR_CHARS.include?(char)
    end

    def bang_delimiter?
      nxt = peek
      nxt.nil? || (operator_char?(nxt) && nxt != "(")
    end

    def group_delimiter?
      nxt = peek
      nxt.nil? || operator_char?(nxt)
    end

    def word_break?(char)
      operator_char?(char) || (@compound && char == "]" && peek == "]")
    end

    def add(type, value = nil, pos: @pos)
      @tokens << Token.new(type: type, value: value, pos: pos)
    end

    def add_word
      start = @pos
      word = scan_word
      add(:word, word, pos: start)
    end

    def skip_blanks
      @pos += 1 while current == " " || current == "\t"
    end

    def skip_comment
      @pos += 1 while current && current != "\n"
    end

    def scan_ampersand
      if peek == "&"
        add(:and_if)
        @pos += 2
      elsif peek == ">" && peek(2) == ">" && @compound
        add(:redir_both_append)
        @pos += 3
      elsif peek == ">"
        add(:redir_both)
        @pos += 2
      else
        add(:amp)
        @pos += 1
      end
    end

    def scan_number_redirect
      start = @pos
      index = start
      index += 1 while digit?(@source[index])

      case @source[index]
      when ">"
        @pos = index
        scan_output_redirect(@source[start...index].to_i)
      when "<"
        @pos = index
        scan_input_redirect(@source[start...index].to_i)
      else
        add_word
      end
    end

    def scan_output_redirect(fd)
      pos = @pos
      if peek == ">"
        add(:redir_append, fd, pos: pos)
        @pos += 2
      elsif peek == "&" && digit?(peek(2))
        target_start = @pos + 2
        index = target_start
        index += 1 while digit?(@source[index])
        target = @source[target_start...index].to_i
        add(:redir_dup, [fd, target], pos: pos)
        @pos = index
      else
        add(:redir_out, fd, pos: pos)
        @pos += 1
      end
    end

    def scan_input_redirect(fd)
      pos = @pos
      if @compound && peek == "<" && peek(2) == "<"
        add(:here_string, fd, pos: pos)
        @pos += 3
      elsif @compound && peek == "<"
        if peek(2) == "-"
          add(:heredoc_strip, fd, pos: pos)
          @pos += 3
        else
          add(:heredoc, fd, pos: pos)
          @pos += 2
        end
      else
        add(:redir_in, fd, pos: pos)
        @pos += 1
      end
    end

    def scan_word
      start = @pos
      parts = []

      while (char = current)
        break if word_break?(char)

        case char
        when "'"
          parts << scan_single_quote
        when '"'
          parts << scan_double_quote
        when "\\"
          scan_escape(parts)
        when "$"
          parts << scan_dollar
        when "@", "!", "?", "+", "*"
          if peek == "("
            raise LexError.new(
              "unsupported syntax: '#{char}('. Use bash -c '...' or a Ruby function.",
              pos: @pos
            )
          end

          append_literal(parts, char)
          @pos += 1
        when "~"
          if parts.empty? && (peek.nil? || peek == "/")
            parts << AST::Part.new(type: :tilde, value: "", pos: @pos)
            @pos += 1
          else
            append_literal(parts, char)
            @pos += 1
          end
        else
          append_literal(parts, char)
          @pos += 1
        end
      end

      AST::Word.new(parts: parts, pos: start)
    end

    def append_literal(parts, text)
      last = parts.last
      if last && last.type == :literal
        last.value << text
      else
        parts << AST::Part.new(type: :literal, value: +text, pos: @pos)
      end
    end

    def scan_escape(parts)
      pos = @pos
      @pos += 1
      char = current
      raise LexError.new("trailing backslash", pos: pos, incomplete: true) if char.nil?

      if char == "\n"
        @pos += 1
      else
        append_literal(parts, char)
        @pos += 1
      end
    end

    def scan_single_quote
      pos = @pos
      @pos += 1
      start = @pos
      @pos += 1 while current && current != "'"
      raise LexError.new("unclosed single quote", pos: pos, incomplete: true) if current.nil?

      value = @source[start...@pos]
      @pos += 1
      AST::Part.new(type: :single, value: value, pos: pos)
    end

    def scan_double_quote
      pos = @pos
      @pos += 1
      inner = []
      buffer = +""

      loop do
        char = current
        raise LexError.new("unclosed double quote", pos: pos, incomplete: true) if char.nil?

        case char
        when '"'
          flush_literal(inner, buffer, pos)
          @pos += 1
          break
        when "\\"
          nxt = peek
          raise LexError.new("unclosed double quote", pos: pos, incomplete: true) if nxt.nil?

          if nxt == "\n"
            @pos += 2
          elsif ["$", "`", '"', "\\"].include?(nxt)
            buffer << nxt
            @pos += 2
          else
            buffer << "\\"
            @pos += 1
          end
        when "$"
          flush_literal(inner, buffer, pos)
          inner << scan_dollar
        else
          buffer << char
          @pos += 1
        end
      end

      AST::Part.new(type: :double, value: inner, pos: pos)
    end

    def flush_literal(parts, buffer, pos)
      return if buffer.empty?

      parts << AST::Part.new(type: :literal, value: buffer.dup, pos: pos)
      buffer.clear
    end

    def scan_dollar
      pos = @pos
      @pos += 1
      char = current

      case char
      when "{"
        scan_braced_variable(pos)
      when "("
        if peek == "("
          scan_arithmetic_expansion(pos)
        else
          scan_command_substitution(pos)
        end
      when "?", "$", "!"
        @pos += 1
        AST::Part.new(type: :special, value: char, pos: pos)
      else
        if char && char.match?(NAME_START)
          start = @pos
          @pos += 1
          @pos += 1 while current && current.match?(NAME_CHAR)
          AST::Part.new(type: :variable, value: @source[start...@pos], pos: pos)
        else
          AST::Part.new(type: :literal, value: "$", pos: pos)
        end
      end
    end

    def scan_braced_variable(pos)
      start = @pos + 1
      index = start
      depth = 0

      while index < @source.length
        if @source[index] == "$" && @source[index + 1] == "{"
          depth += 1
          index += 2
        elsif @source[index] == "}"
          break if depth.zero?

          depth -= 1
          index += 1
        else
          index += 1
        end
      end

      raise LexError.new("unclosed variable expansion", pos: pos, incomplete: true) if index >= @source.length

      content = @source[start...index]
      @pos = index + 1
      AST::Part.new(type: :parameter, value: parse_parameter(content, pos), pos: pos)
    end

    def parse_parameter(content, pos)
      if content.start_with?("#") && content.length > 1
        return AST::Parameter.new(name: content[1..], operator: :length, argument: nil, pos: pos)
      end

      match = /\A([A-Za-z_][A-Za-z0-9_]*)(?:(:-|:=|:\?|:\+|##|#|%%|%|\/\/|\/)(.*))?\z/m.match(content)
      return AST::Parameter.new(name: content, operator: nil, argument: nil, pos: pos) unless match

      operator = case match[2]
                 when ":-" then :default
                 when ":=" then :assign
                 when ":?" then :error
                 when ":+" then :alternate
                 when "##" then :trim_prefix_long
                 when "#" then :trim_prefix_short
                 when "%%" then :trim_suffix_long
                 when "%" then :trim_suffix_short
                 when "//" then :substitute_all
                 when "/" then :substitute
                 end

      AST::Parameter.new(name: match[1], operator: operator, argument: match[3], pos: pos)
    end

    def scan_arithmetic_expansion(pos)
      @pos += 2
      start = @pos
      depth = 0

      while @pos < @source.length
        case @source[@pos]
        when "("
          depth += 1
          @pos += 1
        when ")"
          if depth.zero? && @source[@pos + 1] == ")"
            source = @source[start...@pos].to_s.strip
            @pos += 2
            return AST::Part.new(
              type: :arithmetic,
              value: AST::ArithmeticExpansion.new(source: source, pos: pos),
              pos: pos
            )
          end

          depth -= 1 if depth.positive?
          @pos += 1
        else
          @pos += 1
        end
      end

      raise LexError.new("unclosed arithmetic expansion", pos: pos, incomplete: true)
    end

    def scan_command_substitution(pos)
      content_start = @pos + 1
      index = content_start
      depth = 1
      state = :normal
      states = []

      while index < @source.length
        char = @source[index]

        case state
        when :single
          state = :normal if char == "'"
          index += 1
        when :double
          case char
          when "\\"
            index += 2
          when '"'
            state = :normal
            index += 1
          when "$"
            if @source[index + 1] == "("
              depth += 1
              states.push(:double)
              state = :normal
              index += 2
            else
              index += 1
            end
          else
            index += 1
          end
        else
          case char
          when "\\"
            index += 2
          when "'"
            state = :single
            index += 1
          when '"'
            state = :double
            index += 1
          when "("
            depth += 1
            states.push(:normal)
            index += 1
          when ")"
            depth -= 1
            if depth.zero?
              raw = @source[content_start...index]
              @pos = index + 1
              return AST::Part.new(type: :cmdsub, value: raw, pos: pos)
            end
            state = states.pop || :normal
            index += 1
          when "$"
            if @source[index + 1] == "("
              depth += 1
              states.push(:normal)
              state = :normal
              index += 2
            else
              index += 1
            end
          else
            index += 1
          end
        end
      end

      raise LexError.new("unclosed command substitution", pos: pos, incomplete: true)
    end
  end
end
