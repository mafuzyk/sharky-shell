# frozen_string_literal: true

module Sharky
  class Highlighter
    OPERATOR_PATTERN = /\A(?:\d*>&\d+|\d*>>|\d*>|\d*<|&>|&&|\|\||\||;|&|\(|\))/
    WORD_BREAK = /[\s|&;<>()]/
    COMMENT_BREAK = /[\s|&;<>()]/
    ASSIGNMENT = /\A[A-Za-z_][A-Za-z0-9_]*=/
    FD_DUP = /\A\d*>&\d+\z/
    STYLE_FALLBACKS = {
      command: %i[success],
      quote: %i[hint],
      operator: %i[hint],
      redirect: %i[operator hint],
      path: %i[directory],
      comment: %i[hint]
    }.freeze

    def initialize(theme:, command_exists:)
      @theme = theme || Theme.new
      @command_exists = command_exists || ->(_name) { false }
    end

    def highlight(line)
      text = line.to_s
      error_at = error_index(text)
      output = +""
      index = 0
      command_expected = true
      redirect_target = false

      while index < text.length
        if error_at && index >= error_at
          output << style(:error, text[index..])
          break
        end

        char = text[index]
        if char == " " || char == "\t"
          output << char
          index += 1
          next
        end
        if char == "\n" || char == Editor::NEWLINE_GLYPH
          output << char
          command_expected = true
          redirect_target = false
          index += 1
          next
        end
        if char == "#" && comment_start?(text, index)
          output << style(:comment, text[index..])
          break
        end

        operator = operator_at(text, index)
        if operator
          output << style(operator_style(operator), operator)
          index += operator.length
          if redirect_operator?(operator)
            redirect_target = !fd_dup?(operator)
          elsif operator == ")"
            command_expected = false
            redirect_target = false
          else
            command_expected = true
            redirect_target = false
          end
          next
        end

        start = index
        value, index, quotes = scan_word(text, index)
        role = if redirect_target
                 redirect_target = false
                 :path
               elsif command_expected
                 if assignment?(value)
                   nil
                 else
                   command_expected = false
                   @command_exists.call(value) ? :command : :error
                 end
               elsif path_word?(value)
                 :path
               end
        output << style_word(text[start...index], quotes.map { |range| [range[0] - start, range[1] - start] }, role)
      end

      output
    end

    private

    def error_index(text)
      source = text.gsub(Editor::NEWLINE_GLYPH, "\n")
      Parser.parse(source)
      nil
    rescue ParseError => error
      position = error.pos
      return nil if position.nil?

      if position >= text.length || newline_at?(text, position)
        last_error_start(text)
      else
        position
      end
    rescue StandardError
      nil
    end

    def newline_at?(text, index)
      char = text[index]
      char == "\n" || char == Editor::NEWLINE_GLYPH
    end

    def last_error_start(text)
      index = text.length - 1
      index -= 1 while index >= 0 && (text[index].match?(/\s/) || text[index] == Editor::NEWLINE_GLYPH)
      return nil if index.negative?

      if "|&;<>()".include?(text[index])
        index -= 1 while index.positive? && "|&;<>()".include?(text[index - 1])
      end
      index
    end

    def comment_start?(text, index)
      return true if index.zero?

      text[index - 1].match?(COMMENT_BREAK)
    end

    def operator_at(text, index)
      match = OPERATOR_PATTERN.match(text[index..])
      match && match[0]
    end

    def redirect_operator?(operator)
      operator.match?(/[<>]/)
    end

    def fd_dup?(operator)
      FD_DUP.match?(operator)
    end

    def operator_style(operator)
      redirect_operator?(operator) ? :redirect : :operator
    end

    def assignment?(value)
      ASSIGNMENT.match?(value)
    end

    def path_word?(value)
      value.include?("/") || %w[. ..].include?(value)
    end

    def scan_word(text, index)
      start = index
      value = +""
      quotes = []

      while index < text.length
        char = text[index]
        break if char.match?(WORD_BREAK)

        case char
        when "'"
          finish = text.index("'", index + 1)
          break unless finish

          value << text[(index + 1)...finish]
          quotes << [index, finish + 1]
          index = finish + 1
        when '"'
          finish = closing_double_quote(text, index + 1)
          break unless finish

          value << unescape_double(text[(index + 1)...finish])
          quotes << [index, finish + 1]
          index = finish + 1
        when "\\"
          break if index + 1 >= text.length

          value << text[index + 1]
          index += 2
        else
          value << char
          index += 1
        end
      end

      if index == start
        value << text[index]
        index += 1
      end

      [value, index, quotes]
    end

    def closing_double_quote(text, index)
      while index < text.length
        case text[index]
        when "\\"
          index += 2
        when '"'
          return index
        else
          index += 1
        end
      end
      nil
    end

    def unescape_double(text)
      text.gsub(/\\([\\"$`])/, '\1').gsub("\\\n", "")
    end

    def style_word(raw, quotes, role)
      return raw if role.nil? && quotes.empty?

      output = +""
      index = 0
      while index < raw.length
        quote = quotes.find { |start, finish| index >= start && index < finish }
        if quote
          output << style(:quote, raw[index...quote[1]])
          index = quote[1]
        else
          next_quote = quotes.map(&:first).select { |start| start > index }.min || raw.length
          output << style(role, raw[index...next_quote])
          index = next_quote
        end
      end
      output
    end

    def style(name, text)
      text = text.to_s
      return text if name.nil? || text.empty?

      candidates = [name, *STYLE_FALLBACKS.fetch(name, [])]
      code = candidates.filter_map { |candidate| @theme.styles[candidate] }.first
      return text if code.nil? || @theme.disabled?

      "\e[#{code}m#{text}\e[0m"
    end
  end
end
