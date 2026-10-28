# frozen_string_literal: true

module Sharky
  class Pager
    READ_TIMEOUT = 0.1

    class KeyDecoder < Keys::Decoder
      private

      def parse_csi
        if @buffer.start_with?("\e[Z")
          return [Key.new(type: :tab, value: nil, mods: [:shift]), 3]
        end

        super
      end
    end

    def initialize(terminal:, theme:)
      @terminal = terminal
      @theme = theme || Theme.new
      @input = terminal.respond_to?(:input) ? terminal.input : $stdin
      @output = terminal.respond_to?(:output) ? terminal.output : $stdout
      @output ||= $stdout
    end

    def filter(items, query)
      needle = query.to_s.downcase
      return Array(items) if needle.empty?

      Array(items).select do |item|
        position = 0
        needle.each_char do |character|
          position = item.value.to_s.downcase.index(character, position)
          break unless position

          position += 1
        end
        position
      end
    end

    def select(items, title: "Complete", index: 0, keys: nil, matcher: nil, notice: nil, tab_action: :next)
      select_with_action(
        items,
        title: title,
        index: index,
        keys: keys,
        matcher: matcher,
        notice: notice,
        tab_action: tab_action
      )&.first
    end

    def select_with_action(items, title: "Complete", index: 0, keys: nil, matcher: nil, notice: nil,
                           tab_action: :next)
      items = Array(items)
      return nil if items.empty?
      return nil unless @terminal&.tty?

      @items = items
      @title = title
      @query = ""
      @selected = Integer(index).modulo(items.length)
      @pending = keys
      @keys = []
      @decoder = KeyDecoder.new
      @matcher = matcher
      @notice = notice
      @tab_action = tab_action
      @output.write("\e7")
      render

      loop do
        key = read_key
        return cancel if key.nil?

        case key.type
        when :enter
          chosen = current
          next unless chosen

          clear
          return [chosen, :enter]
        when :escape
          return cancel
        when :alt
          if @pending && key.mods.include?(:ctrl) && key.value == "m"
            @pending.unshift(Key.new(type: :enter))
          end
          return cancel
        when :ctrl
          return cancel if key.value == "c"
        when :up
          move(-1)
        when :down
          move(1)
        when :tab
          if @tab_action == :accept && !key.mods.include?(:shift)
            chosen = current
            next unless chosen

            clear
            return [chosen, :tab]
          end
          key.mods.include?(:shift) ? move(-1) : move(1)
        when :backspace
          @query = @query[0...-1].to_s
          @selected = 0
          render
        when :char
          @query += key.value
          @selected = 0
          render
        end
      end
    end

    private

    def filtered
      @matcher ? @matcher.call(@items, @query) : filter(@items, @query)
    end

    def current
      matches = filtered
      return nil if matches.empty?

      @selected = @selected.clamp(0, matches.length - 1)
      matches[@selected]
    end

    def move(amount)
      count = filtered.length
      return render if count.zero?

      @selected = (@selected + amount) % count
      render
    end

    def cancel
      clear
      nil
    end

    def clear
      @output.write("\e8\e[J")
      @output.flush if @output.respond_to?(:flush)
    end

    def render
      matches = filtered
      @selected = @selected.clamp(0, matches.length - 1) unless matches.empty?
      _columns, rows = terminal_size
      visible_count = [rows - 2, 1].max
      visible = matches.first(visible_count)
      label = @query.empty? ? @title.to_s : "#{@title}  filter: #{@query}"
      label = "#{label}  #{@notice}" if @notice
      lines = [@theme.style(:hint, label)]
      visible.each_with_index do |item, index|
        text = item.value.to_s
        text += "  #{item.description}" if item.description
        lines << (index == @selected ? @theme.style(:selection, text) : text)
      end
      lines << @theme.style(:hint, "no matches") if matches.empty?
      lines << @theme.style(:hint, "#{matches.length - visible.length} more") if matches.length > visible.length

      @output.write("\e8\e[J\n")
      @output.write(lines.join("\r\n"))
      @output.flush if @output.respond_to?(:flush)
    end

    def terminal_size
      columns, rows = @terminal.size
      [columns.to_i, rows.to_i]
    rescue StandardError
      [80, 24]
    end

    def read_key
      loop do
        return @pending.shift if @pending&.any?
        return @keys.shift unless @keys.empty?

        ready = begin
          IO.select([@input], nil, nil, READ_TIMEOUT)
        rescue TypeError, IOError
          true
        end

        unless ready
          @keys.concat(@decoder.flush)
          next
        end

        data = @input.read_nonblock(1)
        @keys.concat(@decoder.feed(data))
      rescue IO::WaitReadable, Errno::EINTR
        next
      rescue EOFError, Errno::EIO
        @keys.concat(@decoder.flush)
        return nil if @keys.empty?
      end
    end
  end
end
