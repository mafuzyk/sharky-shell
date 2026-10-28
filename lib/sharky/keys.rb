# frozen_string_literal: true

module Sharky
  Key = Struct.new(:type, :value, :mods, keyword_init: true)

  module Keys
    CSI = {
      "\e[A" => :up,
      "\e[B" => :down,
      "\e[C" => :right,
      "\e[D" => :left,
      "\e[H" => :home,
      "\e[F" => :end,
      "\e[1~" => :home,
      "\e[4~" => :end,
      "\e[3~" => :delete,
      "\e[Z" => :tab,
      "\e[200~" => :paste_start,
      "\e[201~" => :paste_end
    }.freeze

    SS3 = {
      "A" => :up,
      "B" => :down,
      "C" => :right,
      "D" => :left,
      "H" => :home,
      "F" => :end
    }.freeze

    module_function

    def decode(bytes)
      decoder = Decoder.new
      decoder.feed(bytes) + decoder.flush
    end

    class Decoder
      def initialize
        @buffer = +"".b
      end

      def feed(bytes)
        @buffer << bytes.to_s.b
        keys = []

        loop do
          key, consumed = parse_one
          break if key == :incomplete

          keys << key
          @buffer = @buffer.byteslice(consumed, @buffer.bytesize - consumed) || +"".b
        end

        keys
      end

      def flush
        return [] if @buffer.empty?

        bytes = @buffer
        @buffer = +"".b
        [bytes == "\e" ? key(:escape) : key(:unknown, bytes)]
      end

      private

      def parse_one
        first = @buffer.getbyte(0)
        return :incomplete if first.nil?

        case first
        when 0x1b
          parse_escape
        when 0x0a, 0x0d
          [key(:enter), 1]
        when 0x09
          [key(:tab), 1]
        when 0x08, 0x7f
          [key(:backspace), 1]
        when 0x00..0x1f
          [key(:ctrl, ctrl_value(first), [:ctrl]), 1]
        when 0x20..0x7e
          [key(:char, first.chr(Encoding::UTF_8)), 1]
        else
          parse_utf8(0)
        end
      end

      def parse_escape
        return :incomplete if @buffer.bytesize == 1

        second = @buffer.getbyte(1)
        case second
        when 0x5b # [
          parse_csi
        when 0x4f # O
          parse_ss3
        when 0x5d # ]
          parse_osc
        else
          parse_alt
        end
      end

      def parse_csi
        final = nil
        (2...@buffer.bytesize).each do |index|
          byte = @buffer.getbyte(index)
          if (0x40..0x7e).cover?(byte)
            final = index
            break
          end
        end
        return :incomplete unless final

        sequence = @buffer.byteslice(0, final + 1)
        type = CSI[sequence]
        return [key(type), final + 1] if type

        if sequence == "\e[Z"
          return [key(:tab, nil, [:shift]), final + 1]
        end

        [key(:unknown, sequence), final + 1]
      end

      def parse_ss3
        return :incomplete if @buffer.bytesize < 3

        character = @buffer.byteslice(2, 1)
        type = SS3[character]
        type ? [key(type), 3] : [key(:unknown, @buffer.byteslice(0, 3)), 3]
      end

      def parse_osc
        index = 2
        while index < @buffer.bytesize
          byte = @buffer.getbyte(index)
          if byte == 0x07
            return [key(:unknown, @buffer.byteslice(0, index + 1)), index + 1]
          elsif byte == 0x1b && @buffer.getbyte(index + 1) == 0x5c
            return [key(:unknown, @buffer.byteslice(0, index + 2)), index + 2]
          end

          index += 1
        end
        :incomplete
      end

      def parse_alt
        second = @buffer.getbyte(1)
        return [key(:alt, "\e", [:alt]), 2] if second == 0x1b
        return [key(:alt, "\x7f", [:alt]), 2] if second == 0x7f

        if second < 0x20
          return [key(:alt, ctrl_value(second), %i[alt ctrl]), 2]
        end

        character, length = utf8_at(1)
        return :incomplete if character == :incomplete
        return [key(:unknown, @buffer.byteslice(0, 2)), 2] if character == :invalid

        [key(:alt, character, [:alt]), length + 1]
      end

      def parse_utf8(offset)
        character, length = utf8_at(offset)
        return :incomplete if character == :incomplete
        return [key(:unknown, @buffer.byteslice(offset, 1)), 1] if character == :invalid

        [key(:char, character), length]
      end

      def utf8_at(offset)
        first = @buffer.getbyte(offset)
        length = utf8_length(first)
        return [:invalid, 1] if length == 1 && first >= 0x80
        return [:incomplete, 0] if @buffer.bytesize < offset + length

        string = @buffer.byteslice(offset, length).force_encoding(Encoding::UTF_8)
        return [:invalid, 1] unless string.valid_encoding? && string.length == 1

        [string, length]
      end

      def utf8_length(first)
        case first
        when 0x00..0x7f then 1
        when 0xc2..0xdf then 2
        when 0xe0..0xef then 3
        when 0xf0..0xf4 then 4
        else 1
        end
      end

      def ctrl_value(byte)
        return " " if byte.zero?
        return (byte + 96).chr(Encoding::UTF_8) if (1..26).cover?(byte)

        byte.chr(Encoding::UTF_8)
      end

      def key(type, value = nil, mods = [])
        Key.new(type: type, value: value, mods: mods)
      end
    end
  end
end
