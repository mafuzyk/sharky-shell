module Sharky
  class ParseError < StandardError
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

  class Parser
    RESERVED_WORDS = %w[
      for while until if then elif else fi case esac function select time coproc
      [[ ]] (( ))
    ].freeze

    COMPOUND_STARTERS = %w[if for while until case function].freeze
    REDIRECT_TYPES = [
      :redir_out, :redir_append, :redir_in, :redir_dup, :redir_both,
      :redir_both_append, :heredoc, :heredoc_strip, :here_string
    ].freeze
    REDIRECT_OPS = {
      redir_out: :out,
      redir_append: :append,
      redir_in: :in,
      redir_both: :both,
      redir_both_append: :both_append,
      heredoc: :heredoc,
      heredoc_strip: :heredoc,
      here_string: :here_string
    }.freeze
    HEREDOC_TYPES = [:heredoc, :heredoc_strip].freeze
    NAME = /\A[A-Za-z_][A-Za-z0-9_]*\z/

    def self.parse(source, compound: false)
      new(
        Lexer.new(source, compound: compound).tokenize,
        compound: compound,
        source: source
      ).parse
    rescue LexError => error
      raise ParseError.new(error.message, pos: error.pos, incomplete: error.incomplete?)
    end

    def initialize(tokens, compound: false, source: nil)
      @tokens = tokens
      @compound = compound
      @source = source
      @pos = 0
      @pending_heredocs = []
    end

    def parse
      lines = []
      skip_newlines

      until at?(:eof)
        split_async(parse_list).each { |list| lines << AST::Line.new(list: list) }
        if at?(:newline)
          advance
          skip_newlines
        end
      end

      raise_unclosed_heredoc! unless @pending_heredocs.empty?

      AST::Program.new(lines: lines)
    end

    private

    def current
      @tokens[@pos]
    end

    def peek_token(offset = 1)
      @tokens[@pos + offset]
    end

    def type
      current&.type
    end

    def at?(*types)
      types.include?(type)
    end

    def advance
      token = current
      @pos += 1
      collect_heredocs(token.pos + 1) if token.type == :newline && !@pending_heredocs.empty?
      token
    end

    def skip_newlines
      advance while at?(:newline)
    end

    def split_async(list)
      lines = []
      entries = []

      list.entries.each do |entry|
        entries << entry
        next unless entry.first == :async

        lines << AST::List.new(entries: entries)
        entries = []
      end

      lines << AST::List.new(entries: entries) unless entries.empty?
      lines
    end

    def parse_list
      entries = []

      loop do
        and_or = parse_and_or

        if at?(:semi)
          advance
          entries << [:seq, [and_or]]
          break if at?(:eof, :newline, :rparen)
        elsif at?(:amp)
          advance
          entries << [:async, [and_or]]
          break if at?(:eof, :newline, :rparen)
        elsif at?(:eof, :newline, :rparen)
          entries << [:seq, [and_or]]
          break
        else
          raise syntax_error("unexpected #{describe(current)}")
        end
      end

      AST::List.new(entries: entries)
    end

    def parse_and_or
      first = parse_pipeline
      rest = []

      while at?(:and_if, :or_if)
        op = at?(:and_if) ? :and : :or
        advance
        skip_newlines
        rest << [op, parse_pipeline]
      end

      AST::AndOr.new(first: first, rest: rest)
    end

    def parse_pipeline
      if at?(:bang)
        pos = advance.pos
        return AST::Not.new(command: parse_pipeline, pos: pos)
      end

      commands = [parse_command]
      while consume(:pipe)
        skip_newlines
        commands << parse_command
      end

      AST::Pipeline.new(commands: commands)
    end

    def consume(token_type)
      return false unless at?(token_type)

      advance
      true
    end

    def parse_command
      command = if @compound && at?(:arith_start)
                  parse_arithmetic
                elsif @compound && at?(:test_start)
                  parse_test_command
                elsif compound_command?
                  parse_compound_command
                else
                  check_reserved!
                  at?(:lparen) ? parse_subshell : parse_simple_command
                end

      return command if command.is_a?(AST::Simple)

      redirects = []
      redirects << parse_redirect while REDIRECT_TYPES.include?(type)
      redirects.empty? ? command : AST::Redirected.new(command: command, redirects: redirects)
    end

    def parse_arithmetic
      token = advance
      body_start = token.pos + 2
      body_end = arithmetic_end_index(body_start)

      unless body_end
        raise ParseError.new("unclosed arithmetic command", pos: token.pos, incomplete: true)
      end

      source = @source[body_start...body_end].to_s.strip
      skip_tokens_before(body_end + 2)
      AST::Arithmetic.new(source: source, pos: token.pos)
    end

    def arithmetic_end_index(start)
      depth = 0
      index = start

      while index < @source.length
        case @source[index]
        when "("
          depth += 1
        when ")"
          return index if depth.zero? && @source[index + 1] == ")"

          depth -= 1 if depth.positive?
        end
        index += 1
      end

      nil
    end

    def skip_tokens_before(position)
      @pos += 1 while current && current.pos < position
    end

    def parse_test_command
      token = advance
      body_start = token.pos + 2

      until at?(:test_end)
        raise ParseError.new("unclosed test command", pos: token.pos, incomplete: true) if at?(:eof)

        advance
      end

      source = @source[body_start...current.pos].to_s.strip
      advance
      AST::TestCommand.new(source: source, double_bracket: true, pos: token.pos)
    end

    def parse_compound_command
      case literal_word(current)
      when "if"
        parse_if
      when "for"
        parse_for
      when "while"
        parse_while(false)
      when "until"
        parse_while(true)
      when "case"
        parse_case
      when "function"
        parse_function_keyword
      else
        return parse_group if at?(:lbrace)
        return parse_function_definition if function_definition?

        raise syntax_error("unexpected #{describe(current)}")
      end
    end

    def parse_if
      pos = current.pos
      advance
      branches = []

      loop do
        condition = parse_compound_list(%w[then])
        expect_keyword!("then")
        body = parse_compound_list(%w[elif else fi])
        branches << [condition, body]
        break unless keyword?("elif")

        advance
      end

      else_body = nil
      if keyword?("else")
        advance
        else_body = parse_compound_list(%w[fi])
      end

      expect_keyword!("fi")
      AST::If.new(branches: branches, else_body: else_body, pos: pos)
    end

    def parse_for
      pos = current.pos
      advance
      name = literal_word(current)

      unless name&.match?(NAME)
        raise ParseError.new("unclosed for loop", pos: current.pos, incomplete: true) if at?(:eof)

        raise syntax_error("expected loop variable but found #{describe(current)}")
      end

      advance
      skip_newlines
      words = []

      if keyword?("in")
        advance
        while at?(:word)
          words << convert_word(advance.value)
        end
      end

      skip_compound_separators
      expect_keyword!("do")
      body = parse_compound_list(%w[done])
      expect_keyword!("done")
      AST::For.new(name: name, words: words, body: body, pos: pos)
    end

    def parse_while(until_form)
      pos = current.pos
      advance
      condition = parse_compound_list(%w[do])
      expect_keyword!("do")
      body = parse_compound_list(%w[done])
      expect_keyword!("done")
      AST::While.new(condition: condition, body: body, until_form: until_form, pos: pos)
    end

    def parse_case
      pos = current.pos
      advance

      unless at?(:word)
        raise ParseError.new("unclosed case", pos: current.pos, incomplete: true) if at?(:eof)

        raise syntax_error("expected case word but found #{describe(current)}")
      end

      word = convert_word(advance.value)
      skip_newlines
      expect_keyword!("in")
      skip_compound_separators
      arms = []

      until keyword?("esac")
        raise ParseError.new("unclosed case", pos: current.pos, incomplete: true) if at?(:eof)

        patterns = parse_case_patterns
        body = parse_compound_list(%w[esac], case_body: true)
        arms << [patterns, body]

        if case_break?
          consume_case_terminator
          skip_compound_separators
        elsif keyword?("esac")
          break
        else
          raise syntax_error("unexpected #{describe(current)}")
        end
      end

      expect_keyword!("esac")
      AST::Case.new(word: word, arms: arms, pos: pos)
    end

    def parse_case_patterns
      patterns = []
      advance if at?(:lparen)

      loop do
        unless at?(:word)
          raise ParseError.new("unclosed case pattern", pos: current.pos, incomplete: true) if at?(:eof)

          raise syntax_error("expected case pattern but found #{describe(current)}")
        end

        patterns << convert_word(advance.value)
        break unless at?(:pipe)

        advance
        skip_newlines
      end

      unless at?(:rparen)
        raise ParseError.new("unclosed case pattern", pos: current.pos, incomplete: true) if at?(:eof)

        raise syntax_error("expected ) after case pattern but found #{describe(current)}")
      end
      advance

      patterns
    end

    def consume_case_terminator
      if at?(:case_break, :case_continue)
        advance
      else
        advance
        advance
      end
    end

    def parse_function_definition
      pos = current.pos
      name = literal_word(current)
      advance
      advance
      advance
      skip_newlines
      body = parse_function_body
      AST::FunctionDef.new(name: name, body: body, pos: pos)
    end

    def parse_function_keyword
      pos = current.pos
      advance
      name = literal_word(current)

      unless name
        raise ParseError.new("unclosed function definition", pos: current.pos, incomplete: true) if at?(:eof)

        raise syntax_error("expected function name but found #{describe(current)}")
      end

      advance
      if at?(:lparen) && peek_token&.type == :rparen
        advance
        advance
      end
      skip_newlines
      body = parse_function_body
      AST::FunctionDef.new(name: name, body: body, pos: pos)
    end

    def parse_function_body
      unless at?(:lbrace)
        if at?(:eof)
          raise ParseError.new("unclosed function definition", pos: current.pos, incomplete: true)
        end

        raise syntax_error("expected { after function definition but found #{describe(current)}")
      end

      parse_group.list
    end

    def parse_group
      pos = current.pos
      advance
      list = parse_compound_list([], token_terminators: [:rbrace])
      advance
      AST::Group.new(list: list, pos: pos)
    end

    def parse_compound_list(terminators, token_terminators: [], case_body: false)
      entries = []
      skip_compound_separators(case_body: case_body)

      until at?(:eof) || compound_list_terminator?(terminators, token_terminators) ||
            (case_body && case_break?)
        and_or = parse_and_or

        if case_body && case_break?
          entries << [:seq, [and_or]]
          break
        elsif at?(:semi, :amp)
          entries << [at?(:amp) ? :async : :seq, [and_or]]
          advance
          skip_compound_separators(case_body: case_body)
        elsif at?(:newline)
          entries << [:seq, [and_or]]
          advance
          skip_compound_separators(case_body: case_body)
        elsif at?(:eof) || compound_list_terminator?(terminators, token_terminators)
          entries << [:seq, [and_or]]
          break
        else
          raise syntax_error("unexpected #{describe(current)}")
        end
      end

      if at?(:eof)
        raise ParseError.new("unclosed compound command", pos: current.pos, incomplete: true)
      end

      AST::List.new(entries: entries)
    end

    def compound_list_terminator?(terminators, token_terminators)
      terminators.any? { |word| keyword?(word) } || token_terminators.include?(type)
    end

    def case_break?
      at?(:case_break, :case_continue) || (at?(:semi) && peek_token&.type == :semi)
    end

    def skip_compound_separators(case_body: false)
      loop do
        break if case_body && case_break?
        break unless at?(:semi, :newline)

        advance
      end
    end

    def expect_keyword!(word)
      return advance if keyword?(word)

      if at?(:eof)
        raise ParseError.new("unclosed compound command", pos: current.pos, incomplete: true)
      end

      raise syntax_error("expected #{word} but found #{describe(current)}")
    end

    def parse_simple_command
      assignments = []
      words = []
      redirects = []

      loop do
        if at?(:word)
          token = current
          word = token.value
          assignment = assignment_for(word)

          if words.empty? && assignment
            advance
            assignment.array = parse_array_elements if at?(:lparen)
            assignments << assignment
          else
            check_reserved_word!(token) if words.empty?
            advance
            words << convert_word(word)
          end
        elsif !words.empty? && at?(:bang, :lbrace)
          token = advance
          text = token.type == :bang ? "!" : "{"
          part = AST::Part.new(type: :literal, value: text, pos: token.pos)
          words << AST::Word.new(parts: [part], pos: token.pos)
        elsif REDIRECT_TYPES.include?(type)
          redirects << parse_redirect
        else
          break
        end
      end

      if assignments.empty? && words.empty? && redirects.empty?
        if at?(:eof, :newline)
          raise ParseError.new("unexpected end of input", pos: current.pos, incomplete: true)
        end

        raise syntax_error("unexpected #{describe(current)}")
      end

      AST::Simple.new(assignments: assignments, words: words, redirects: redirects)
    end

    def parse_subshell
      advance
      skip_newlines
      list = parse_list
      while at?(:newline)
        skip_newlines
        break if at?(:rparen, :eof)

        list.entries.concat(parse_list.entries)
      end

      unless at?(:rparen)
        raise ParseError.new("unclosed subshell", pos: current.pos, incomplete: true) if at?(:eof, :newline)

        raise syntax_error("expected ) but found #{describe(current)}")
      end
      advance

      AST::Subshell.new(list: list)
    end

    def parse_redirect
      check_reserved_redirect!
      token = advance

      if token.type == :redir_dup
        fd, target = token.value
        return AST::Redirect.new(fd: fd || 1, op: :dup, target: target)
      end

      target = expect_redirect_word

      if HEREDOC_TYPES.include?(token.type)
        delimiter, quoted = heredoc_delimiter(target)
        document = AST::HereDoc.new(
          delimiter: delimiter,
          strip_tabs: token.type == :heredoc_strip,
          quoted: quoted,
          body: nil,
          pos: token.pos
        )
        @pending_heredocs << document
        return AST::Redirect.new(fd: token.value || 0, op: :heredoc, target: document)
      end

      op = REDIRECT_OPS.fetch(token.type)
      default_fd = [:in, :here_string].include?(op) ? 0 : 1
      fd = token.value || ([:both, :both_append].include?(op) ? nil : default_fd)
      AST::Redirect.new(fd: fd, op: op, target: convert_word(target))
    end

    def heredoc_delimiter(word)
      raw_end = current&.pos || @source.length
      raw = @source[word.pos...raw_end].to_s.strip
      quoted = raw.include?("'") || raw.include?('"') || raw.include?("\\")
      if raw.length >= 2 && ["'", '"'].include?(raw[0]) && raw[-1] == raw[0]
        raw = raw[1...-1]
      end
      delimiter = raw.gsub(/\\(.)/m, '\1')
      [delimiter, quoted]
    end

    def expect_redirect_word
      raise ParseError.new("missing redirect target", pos: current.pos, incomplete: true) if at?(:eof, :newline)
      raise syntax_error("missing redirect target") unless at?(:word)

      advance.value
    end

    def collect_heredocs(body_start)
      pending = @pending_heredocs
      @pending_heredocs = []

      pending.each do |document|
        body, following = read_heredoc(document, body_start)
        document.body = body
        @pos = following
        body_start = @tokens[following].pos + 1
      end
    end

    def read_heredoc(document, body_start)
      line_start = body_start
      while line_start <= @source.length
        line_end = @source.index("\n", line_start)
        content_end = line_end || @source.length
        line = @source[line_start...content_end]
        line = line.sub(/\A\t+/, "") if document.strip_tabs

        if line == document.delimiter
          body = @source[body_start...line_start]
          body = body.gsub(/^\t+/, "") if document.strip_tabs
          following = @tokens.index { |token| token.pos >= line_start && token.type == :newline }
          following ||= @tokens.index { |token| token.type == :eof }
          return [body, following]
        end

        break unless line_end

        line_start = line_end + 1
      end

      raise_unclosed_heredoc!(document)
    end

    def raise_unclosed_heredoc!(document = nil)
      document ||= @pending_heredocs.first
      raise ParseError.new(
        "unclosed here-document '#{document.delimiter}'",
        pos: document.pos,
        incomplete: true
      )
    end

    def parse_array_elements
      advance
      skip_newlines
      elements = []

      until at?(:rparen)
        if at?(:eof)
          raise ParseError.new("unclosed array assignment", pos: current.pos, incomplete: true)
        end

        if at?(:word)
          elements << convert_word(advance.value)
        elsif at?(:newline)
          advance
          skip_newlines
        else
          raise syntax_error("expected array element but found #{describe(current)}")
        end
      end
      advance
      elements
    end

    def assignment_for(word)
      first = word.parts.first
      return nil unless first && first.type == :literal

      index = first.value.index("=")
      return nil unless index

      prefix = first.value[0...index]
      element_index = nil
      append = false

      if (match = /\A([A-Za-z_][A-Za-z0-9_]*)\[(\d+)\]\z/.match(prefix))
        name = match[1]
        element_index = match[2]
      elsif prefix.end_with?("+")
        name = prefix[0...-1]
        append = true
      else
        name = prefix
      end
      return nil unless name.match?(NAME)

      rest = first.value[(index + 1)..]
      parts = []
      parts << AST::Part.new(type: :literal, value: rest, pos: first.pos + index + 1) unless rest.empty?
      parts.concat(word.parts[1..])

      AST::Assignment.new(
        name: name,
        value: AST::Word.new(parts: parts, pos: first.pos + index + 1),
        index: element_index,
        append: append
      )
    end

    def convert_word(word)
      AST::Word.new(parts: word.parts.map { |part| convert_part(part) }, pos: word.pos)
    end

    def convert_part(part)
      case part.type
      when :cmdsub
        AST::Part.new(type: :cmdsub, value: self.class.parse(part.value, compound: @compound), pos: part.pos)
      when :double
        inner = part.value.map { |child| convert_part(child) }
        AST::Part.new(type: :double, value: inner, pos: part.pos)
      else
        part
      end
    end

    def check_reserved!
      return if current.nil?

      check_reserved_word!(current)
      following = peek_token

      if at?(:lparen) && following&.type == :lparen && following.pos == current.pos + 1
        unsupported_syntax!("((", current.pos)
      end

      check_reserved_redirect! if [:redir_in, :redir_out].include?(type)
    end

    def check_reserved_redirect!
      following = peek_token
      return unless following

      if following.type == :lparen && following.pos == current.pos + 1
        unsupported_syntax!(type == :redir_in ? "<(" : ">(", current.pos)
      end

      if type == :redir_in && following.type == :redir_in && following.pos == current.pos + 1
        unsupported_syntax!("<<", current.pos)
      end
    end

    def check_reserved_word!(token)
      return unless reserved_word?(token)

      words = token.value.parts.map(&:value).join
      unsupported_syntax!(words, token.pos)
    end

    def unsupported_syntax!(syntax, pos)
      raise ParseError.new(
        "unsupported syntax: '#{syntax}'. Use bash -c '...' or a Ruby function.",
        pos: pos
      )
    end

    def compound_command?
      return false unless @compound

      at?(:lbrace) || COMPOUND_STARTERS.include?(literal_word(current)) || function_definition?
    end

    def function_definition?
      word = at?(:word) ? literal_word(current) : nil
      return false if word.nil? || word.include?("=")

      peek_token&.type == :lparen && peek_token(2)&.type == :rparen
    end

    def keyword?(word)
      literal_word(current) == word
    end

    def literal_word(token)
      return nil unless token&.type == :word

      parts = token.value.parts
      return nil unless parts.length == 1 && parts.first.type == :literal

      parts.first.value
    end

    def reserved_word?(token)
      RESERVED_WORDS.include?(literal_word(token))
    end

    def describe(token)
      return "end of input" if token.nil? || token.type == :eof
      return token.value.parts.map(&:value).join if token.type == :word

      token.type.to_s
    end

    def syntax_error(message)
      ParseError.new(message, pos: current&.pos)
    end
  end
end
