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

    COMPOUND_STARTERS = %w[if for while until].freeze
    REDIRECT_TYPES = [:redir_out, :redir_append, :redir_in, :redir_dup, :redir_both].freeze
    REDIRECT_OPS = {
      redir_out: :out,
      redir_append: :append,
      redir_in: :in,
      redir_both: :both
    }.freeze
    NAME = /\A[A-Za-z_][A-Za-z0-9_]*\z/

    def self.parse(source, compound: false)
      new(Lexer.new(source, compound: compound).tokenize, compound: compound).parse
    rescue LexError => error
      raise ParseError.new(error.message, pos: error.pos, incomplete: error.incomplete?)
    end

    def initialize(tokens, compound: false)
      @tokens = tokens
      @compound = compound
      @pos = 0
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
        rest << [op, parse_pipeline]
      end

      AST::AndOr.new(first: first, rest: rest)
    end

    def parse_pipeline
      commands = [parse_command]
      commands << parse_command while consume(:pipe)

      AST::Pipeline.new(commands: commands)
    end

    def consume(token_type)
      return false unless at?(token_type)

      advance
      true
    end

    def parse_command
      return parse_compound_command if compound_command?

      check_reserved!

      if at?(:lparen)
        parse_subshell
      else
        parse_simple_command
      end
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
      else
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

    def parse_compound_list(terminators)
      entries = []
      skip_compound_separators

      until at?(:eof) || terminators.any? { |word| keyword?(word) }
        and_or = parse_and_or

        if at?(:semi, :amp)
          entries << [at?(:amp) ? :async : :seq, [and_or]]
          advance
          skip_compound_separators
        elsif at?(:newline)
          entries << [:seq, [and_or]]
          advance
          skip_compound_separators
        elsif at?(:eof) || terminators.any? { |word| keyword?(word) }
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

    def skip_compound_separators
      advance while at?(:semi, :newline)
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

          if words.empty? && assignments.empty? && assignment
            advance
            assignments << assignment
          else
            check_reserved_word!(token) if words.empty?
            advance
            words << convert_word(word)
          end
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
      list = parse_list

      unless at?(:rparen)
        raise ParseError.new("unclosed subshell", pos: current.pos, incomplete: true) if at?(:eof, :newline)

        raise syntax_error("expected ) but found #{describe(current)}")
      end
      advance

      redirects = []
      redirects << parse_redirect while REDIRECT_TYPES.include?(type)
      subshell = AST::Subshell.new(list: list)
      attach_redirects(subshell, redirects)
      subshell
    end

    def attach_redirects(subshell, redirects)
      return if redirects.empty?

      entry = subshell.list.entries.last
      return unless entry

      command = entry[1].first.commands.last
      case command
      when AST::Simple
        command.redirects.concat(redirects)
      when AST::Subshell
        attach_redirects(command, redirects)
      end
    end

    def parse_redirect
      check_reserved_redirect!
      token = advance

      if token.type == :redir_dup
        fd, target = token.value
        return AST::Redirect.new(fd: fd || 1, op: :dup, target: target)
      end

      target = expect_redirect_word
      op = REDIRECT_OPS.fetch(token.type)
      default_fd = op == :in ? 0 : 1
      fd = token.value || (op == :both ? nil : default_fd)
      AST::Redirect.new(fd: fd, op: op, target: convert_word(target))
    end

    def expect_redirect_word
      raise ParseError.new("missing redirect target", pos: current.pos, incomplete: true) if at?(:eof, :newline)
      raise syntax_error("missing redirect target") unless at?(:word)

      advance.value
    end

    def assignment_for(word)
      first = word.parts.first
      return nil unless first && first.type == :literal

      index = first.value.index("=")
      return nil unless index

      name = first.value[0...index]
      return nil unless name.match?(NAME)

      rest = first.value[(index + 1)..]
      parts = []
      parts << AST::Part.new(type: :literal, value: rest, pos: first.pos + index + 1) unless rest.empty?
      parts.concat(word.parts[1..])

      AST::Assignment.new(
        name: name,
        value: AST::Word.new(parts: parts, pos: first.pos + index + 1)
      )
    end

    def convert_word(word)
      AST::Word.new(parts: word.parts.map { |part| convert_part(part) }, pos: word.pos)
    end

    def convert_part(part)
      case part.type
      when :cmdsub
        AST::Part.new(type: :cmdsub, value: self.class.parse(part.value), pos: part.pos)
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
      @compound && COMPOUND_STARTERS.include?(literal_word(current))
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
