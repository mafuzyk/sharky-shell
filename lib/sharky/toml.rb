# frozen_string_literal: true

module Sharky
  # Minimal TOML reader for theme files.
  #
  # The full TOML spec is large and would need a dependency. Themes need
  # flat tables of string values plus the prompt layout tables, so this
  # reader supports:
  #   - # comments and blank lines
  #   - bare keys: [A-Za-z0-9_-]+
  #   - basic strings ("...") with the standard escapes
  #   - literal strings ('...')
  #   - tables ([styles]), arrays of tables ([[lines]]) and dotted names
  #   - integers and booleans inside tables, for layout fields
  #
  # Top-level flat values stay string-only so existing theme files keep
  # their meaning. Anything else raises ParseError with a file/line
  # location. Keeping the parser small avoids pulling RubyGems into the
  # shell's startup path.
  module Toml
    class ParseError < StandardError; end

    BARE_KEY = /\A[A-Za-z0-9_-]+\z/
    BASIC_ESCAPES = {
      "b" => "\b", "t" => "\t", "n" => "\n", "f" => "\f", "r" => "\r",
      '"' => '"', "\\" => "\\"
    }.freeze

    module_function

    def parse(source, path: nil)
      result = {}
      table = result
      tables = {}

      source.to_s.each_line.with_index(1) do |raw, line_number|
        text = strip_comment(raw).strip
        next if text.empty?

        where = location(path, line_number)
        if text.start_with?("[[")
          table = open_array_table(result, text, where)
          next
        elsif text.start_with?("[")
          table = open_table(result, text, where, tables)
          next
        end

        key, separator, raw_value = text.partition("=")
        raise ParseError, "#{where}: expected key = value" if separator.empty?

        key = key.strip
        raise ParseError, "#{where}: invalid key #{key.inspect}" unless key.match?(BARE_KEY)
        raise ParseError, "#{where}: duplicate key #{key.inspect}" if table.key?(key)

        table[key] = parse_value(raw_value.strip, where, flat: table.equal?(result))
      end

      result
    end

    def strip_comment(line)
      quote = nil
      escaped = false

      line.each_char.with_index do |char, index|
        if quote
          if quote == '"' && char == "\\" && !escaped
            escaped = true
            next
          end
          if char == quote && !escaped
            quote = nil
          end
          escaped = false
        elsif char == '"' || char == "'"
          quote = char
        elsif char == "#"
          return line[0...index]
        end
      end

      line
    end

    def open_table(result, text, where, tables)
      raise ParseError, "#{where}: unterminated table header" unless text.end_with?("]")

      name = text[1...-1].strip
      parts = table_parts(name, where)
      key = parts.join(".")
      raise ParseError, "#{where}: duplicate table #{name.inspect}" if tables.key?(key)

      table = result
      parts.each do |part|
        table[part] ||= {}
        raise ParseError, "#{where}: table #{part.inspect} is not a table" unless table[part].is_a?(Hash)

        table = table[part]
      end

      tables[key] = true
      table
    end

    def open_array_table(result, text, where)
      raise ParseError, "#{where}: unterminated table header" unless text.end_with?("]]")

      name = text[2...-2].strip
      parts = table_parts(name, where)
      parent = result
      parts[0...-1].each do |part|
        parent[part] ||= {}
        raise ParseError, "#{where}: table #{part.inspect} is not a table" unless parent[part].is_a?(Hash)

        parent = parent[part]
      end

      key = parts.last
      parent[key] ||= []
      raise ParseError, "#{where}: table #{key.inspect} is not an array of tables" unless parent[key].is_a?(Array)

      entry = {}
      parent[key] << entry
      entry
    end

    def table_parts(name, where)
      raise ParseError, "#{where}: empty table name" if name.empty?

      parts = name.split(".", -1).map(&:strip)
      parts.each do |part|
        raise ParseError, "#{where}: invalid table name #{part.inspect}" unless part.match?(BARE_KEY)
      end
      parts
    end

    def parse_value(value, where, flat:)
      if value.start_with?('"')
        parse_basic_string(value, where)
      elsif value.start_with?("'")
        parse_literal_string(value, where)
      elsif !flat && value == "true"
        true
      elsif !flat && value == "false"
        false
      elsif !flat && value.match?(/\A[+-]?\d+\z/)
        value.to_i
      elsif flat
        raise ParseError, "#{where}: theme values must be strings"
      else
        raise ParseError, "#{where}: theme values must be strings, integers or booleans"
      end
    end

    def parse_basic_string(value, where)
      result = +""
      index = 1

      while index < value.length
        char = value[index]

        if char == "\\"
          index += 1
          raise ParseError, "#{where}: unterminated escape" if index >= value.length

          escape = value[index]
          if escape == "u" || escape == "U"
            length = escape == "u" ? 4 : 8
            hex = value[index + 1, length].to_s
            raise ParseError, "#{where}: invalid Unicode escape" unless hex.match?(/\A[0-9A-Fa-f]{#{length}}\z/)

            result << [hex.to_i(16)].pack("U")
            index += length + 1
          elsif BASIC_ESCAPES.key?(escape)
            result << BASIC_ESCAPES[escape]
            index += 1
          else
            raise ParseError, "#{where}: unsupported escape \\#{escape}"
          end
        elsif char == '"'
          raise ParseError, "#{where}: trailing text after string" unless index == value.length - 1

          return result
        else
          result << char
          index += 1
        end
      end

      raise ParseError, "#{where}: unterminated string"
    end

    def parse_literal_string(value, where)
      closing = value.index("'", 1)
      raise ParseError, "#{where}: unterminated string" unless closing
      raise ParseError, "#{where}: trailing text after string" unless closing == value.length - 1

      value[1...closing]
    end

    def location(path, line_number)
      prefix = path ? "#{path}:" : ""
      "#{prefix}line #{line_number}"
    end
  end
end
