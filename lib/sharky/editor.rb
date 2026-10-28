# frozen_string_literal: true

module Sharky
  class Editor
    NEWLINE_GLYPH = "⏎"
    SELECT_TIMEOUT = 0.1
    DEFAULT_SHELL = Struct.new(:env, :cwd)

    DEFAULT_BINDINGS = {
      "ctrl-a" => :beginning_of_line,
      "ctrl-b" => :backward_char,
      "ctrl-c" => :clear_line,
      "ctrl-d" => :delete_char,
      "ctrl-e" => :end_of_line,
      "ctrl-f" => :forward_char,
      "ctrl-k" => :kill_line,
      "ctrl-l" => :clear_screen,
      "ctrl-r" => :history_search,
      "ctrl-t" => :file_search,
      "ctrl-u" => :kill_to_start,
      "ctrl-w" => :kill_word,
      "ctrl-y" => :yank,
      "ctrl-z" => :noop,
      "alt-b" => :backward_word,
      "alt-f" => :forward_word
    }.freeze

    def initialize(terminal:, history:, prompt:, theme:, runtime: nil, output: nil, highlighter: nil,
                   completion: nil, shell: nil)
      @terminal = terminal
      @history = history
      @prompt = prompt
      @theme = theme || Theme.new
      @runtime = runtime
      @shell = shell || DEFAULT_SHELL.new(ENV.to_h, Dir.pwd)
      @completion = completion
      @completion ||= Completion.new(runtime: runtime, shell: @shell) if runtime
      @input = terminal.respond_to?(:input) ? terminal.input : $stdin
      @output = output || (terminal.respond_to?(:output) ? terminal.output : $stdout) || $stdout
      @highlighter = highlighter || Highlighter.new(theme: @theme, command_exists: method(:command_exists))
      @command_cache = {}
      @buffer = LineBuffer.new
      @decoder = Pager::KeyDecoder.new
      @scroll = 0
      @winch = false
      @old_winch = nil
      @pasting = false
      @history_entries = nil
      @history_index = 0
      @history_prefix = ""
      @suggestion = nil
      @suggestion_key = nil
      @pending_keys = []
    end

    def readline
      return nil unless @terminal.tty?

      reset_state
      @prompt_lines = prompt_text.split("\n", -1)
      install_winch
      @terminal.with_raw do
        redraw
        loop do
          chunk = read_chunk
          case chunk
          when :winch
            @winch = false
            redraw
            next
          when :eof
            @output.write("\r\n")
            return nil
          when :timeout
            next
          end

          @pending_keys.concat(@decoder.feed(chunk))
          until @pending_keys.empty?
            result = handle(@pending_keys.shift)
            case result
            when :submit
              @output.write("\r\n")
              return @buffer.text
            when :eof
              @output.write("\r\n")
              return nil
            when :handled
              redraw
            end
          end
        end
      end
    ensure
      restore_winch
    end

    private

    def reset_state
      @buffer = LineBuffer.new
      @decoder = Pager::KeyDecoder.new
      @scroll = 0
      @pasting = false
      @suggestion = nil
      @suggestion_key = nil
      @pending_keys = []
      @drawn_prompt_height = 0
      reset_history
    end

    def install_winch
      return unless Signal.list.key?("WINCH")

      @winch = false
      @old_winch = Signal.trap("WINCH") { @winch = true }
    rescue ArgumentError
      @old_winch = nil
    end

    def restore_winch
      return unless @old_winch

      Signal.trap("WINCH", @old_winch)
      @old_winch = nil
    rescue ArgumentError
      nil
    end

    def read_chunk
      return :winch if @winch

      ready = begin
        IO.select([@input], nil, nil, SELECT_TIMEOUT)
      rescue TypeError, IOError, Errno::EBADF
        true
      end
      return :winch if @winch
      return :timeout unless ready

      data = @input.read_nonblock(4096)
      data.nil? ? :timeout : data
    rescue IO::WaitReadable, Errno::EINTR
      :timeout
    rescue EOFError, Errno::EIO
      :eof
    end

    def handle(key)
      return :none if key.nil?

      return handle_paste(key) if @pasting

      action = binding_for(key)
      return perform(action) if action
      return accept_suggestion_word if alt_right?(key)

      case key.type
      when :char
        if key.value == " " && expand_abbreviation(" ")
          :handled
        else
          insert(key.value)
        end
      when :enter
        enter
      when :backspace
        edit { @buffer.backspace }
      when :delete
        edit { @buffer.delete }
      when :left
        edit { @buffer.left }
      when :right
        return accept_suggestion if suggestion

        edit { @buffer.right }
      when :home
        edit { @buffer.home }
      when :end
        edit { @buffer.end }
      when :up
        history_up
      when :down
        history_down
      when :tab
        complete(reverse: key.mods.include?(:shift))
      when :escape
        :handled
      when :paste_start
        @pasting = true
        :handled
      when :paste_end
        :handled
      else
        :handled
      end
    end

    def handle_paste(key)
      case key.type
      when :paste_end
        @pasting = false
      when :char
        @buffer.insert(key.value)
      when :enter
        @buffer.insert("\n")
      when :tab
        @buffer.insert("\t")
      when :backspace
        @buffer.backspace
      end
      :handled
    end

    def insert(value)
      reset_history
      @buffer.insert(value)
      :handled
    end

    def edit
      reset_history
      yield
      :handled
    end

    def complete(reverse: false)
      return :none unless @completion

      start, word = @completion.token(@buffer.text, @buffer.cursor)
      candidates = @completion.candidates(@buffer.text, @buffer.cursor)
      return :handled if candidates.empty?

      if candidates.length == 1
        replace_token(start, candidates.first.value)
        return :handled
      end

      prefix = longest_common_prefix(candidates.map(&:value))
      replace_token(start, prefix) if prefix.length > word.length

      pager = Pager.new(terminal: @terminal, theme: @theme)
      @pending_keys.concat(@decoder.flush)
      chosen = pager.select(candidates, title: "Complete", index: reverse ? -1 : 0, keys: @pending_keys)
      replace_token(start, chosen.value) if chosen
      :handled
    end

    def history_search
      entries = @history ? @history.entries.reverse : []
      return :handled if entries.empty?

      candidates = entries.map { |entry| Completion::Candidate.new(value: entry) }
      selected, action = pick(candidates, title: "History", matcher: method(:history_sort))
      return :handled unless selected

      @buffer.replace(selected.value)
      reset_history
      action == :enter ? :submit : :handled
    end

    def file_search
      files, capped = Fuzzy.files(@shell.cwd || Dir.pwd)
      return :handled if files.empty?

      candidates = files.map do |path|
        Completion::Candidate.new(value: path, description: path.end_with?(File::SEPARATOR) ? "dir" : "file")
      end
      notice = "scan capped at #{Fuzzy::FILE_LIMIT} entries" if capped
      selected, = pick(candidates, title: "Files", matcher: method(:fuzzy_sort), notice: notice)
      return :handled unless selected

      @buffer.insert(selected.value)
      reset_history
      :handled
    end

    def pick(items, title:, matcher:, notice: nil)
      @pending_keys.concat(@decoder.flush)
      pager = Pager.new(terminal: @terminal, theme: @theme)
      pager.select_with_action(
        items,
        title: title,
        keys: @pending_keys,
        matcher: matcher,
        notice: notice,
        tab_action: :accept
      )
    end

    def history_sort(items, query)
      return items if query.empty?

      items.each_with_index.filter_map do |item, index|
        value = Fuzzy.score(query, item.value)
        next unless value.positive?

        recency = (items.length - index).to_f / (items.length + 1)
        [item, value + recency, index]
      end.sort_by { |item, value, index| [-value, index] }.map(&:first)
    end

    def fuzzy_sort(items, query)
      return items if query.empty?

      items.each_with_index.filter_map do |item, index|
        value = Fuzzy.score(query, item.value)
        [item, value, index] if value.positive?
      end.sort_by { |item, value, index| [-value, index] }.map(&:first)
    end

    def longest_common_prefix(values)
      values.reduce do |prefix, value|
        limit = [prefix.length, value.length].min
        index = 0
        index += 1 while index < limit && prefix[index] == value[index]
        prefix[0...index]
      end.to_s
    end

    def replace_token(start, value)
      before = @buffer.text[0...start]
      after = @buffer.text[@buffer.cursor..].to_s
      @buffer.replace(before + value + after)
      @buffer.home
      (start + value.length).times { @buffer.right }
      reset_history
    end

    def enter
      if @pasting
        @buffer.insert("\n")
        return :handled
      end

      expand_abbreviation
      if @buffer.text.empty? || complete?
        :submit
      else
        @buffer.insert("\n")
        :handled
      end
    end

    def complete?
      Parser.parse(@buffer.text)
      true
    rescue ParseError => error
      !error.incomplete?
    rescue StandardError
      true
    end

    def expand_abbreviation(separator = "")
      prefix = @buffer.text[0...@buffer.cursor]
      match = /\A(\s*)(\S+)\z/.match(prefix)
      return false unless match
      return false unless @buffer.cursor == @buffer.text.length

      expansion = abbreviations[match[2]]
      return false unless expansion

      separator = "" if expansion.end_with?(" ")
      @buffer.replace(match[1] + expansion + separator)
      reset_history
      true
    end

    def abbreviations
      (@runtime&.abbreviations || {}).to_h
    end

    def binding_for(key)
      name = key_name(key)
      custom = (@runtime&.bindings || {}).find { |candidate, _| normalize_binding(candidate) == name }
      return custom[1].to_sym if custom && custom[1]

      DEFAULT_BINDINGS[name]
    end

    def key_name(key)
      case key.type
      when :ctrl
        "ctrl-#{key.value}"
      when :alt
        "alt-#{key.value}"
      when :char
        key.value.to_s
      when :tab
        Array(key.mods).include?(:shift) ? "shift-tab" : "tab"
      else
        key.type.to_s
      end
    end

    def normalize_binding(name)
      name.to_s.downcase
          .sub(/\A\\c-/, "ctrl-")
          .sub(/\A\\m-/, "alt-")
          .sub(/\Ac-/, "ctrl-")
          .sub(/\Am-/, "alt-")
    end

    def perform(action)
      case action
      when :beginning_of_line
        edit { @buffer.home }
      when :end_of_line
        edit { @buffer.end }
      when :backward_char
        edit { @buffer.left }
      when :forward_char
        return accept_suggestion if suggestion

        edit { @buffer.right }
      when :backward_word
        edit { @buffer.word_left }
      when :forward_word
        edit { @buffer.word_right }
      when :kill_line
        edit { @buffer.kill_to_end }
      when :kill_to_start
        edit { @buffer.kill_to_start }
      when :kill_word
        edit { @buffer.kill_word }
      when :yank
        edit { @buffer.yank }
      when :clear_line
        reset_history
        @buffer.clear
        :handled
      when :delete_char
        return :eof if @buffer.empty?

        edit { @buffer.delete }
      when :clear_screen
        @drawn_prompt_height = 0
        @output.write("\e[2J\e[H")
        :handled
      when :complete
        complete
      when :history_search
        history_search
      when :file_search
        file_search
      when :noop
        :handled
      when :accept_suggestion
        accept_suggestion
      else
        :none
      end
    end

    def history_up
      if @history_entries.nil?
        @history_prefix = @buffer.text[0...@buffer.cursor]
        @history_entries = @history ? @history.search(@history_prefix) : []
        @history_index = -1
      end
      return :handled if @history_entries.empty?

      @history_index += 1
      if @history_index < @history_entries.length
        @buffer.replace(@history_entries[@history_index])
      else
        @buffer.replace(@history_prefix)
        @history_entries = nil
      end
      :handled
    end

    def history_down
      return :handled unless @history_entries

      @history_index += 1
      if @history_index < @history_entries.length
        @buffer.replace(@history_entries[@history_index])
      else
        @buffer.replace(@history_prefix)
        @history_entries = nil
      end
      :handled
    end

    def reset_history
      @history_entries = nil
      @history_index = 0
      @history_prefix = ""
    end

    def redraw
      columns, = @terminal.size
      columns = 80 unless columns.to_i.positive?
      prompt_lines = (@prompt_lines || prompt_text.split("\n", -1)).map do |line|
        @terminal.width(strip_ansi(line)) > columns ? slice_ansi(line, 0, columns) : line
      end
      prompt = prompt_lines.last.to_s
      prompt_width = @terminal.width(strip_ansi(prompt))
      available = [columns - prompt_width - 1, 1].max
      display = display_text
      total_width = @terminal.width(display)
      cursor_width = @terminal.width(display_text[0...@buffer.cursor])

      if total_width <= available
        @scroll = 0
      else
        @scroll = 0 if @scroll > total_width
        @scroll = cursor_width if cursor_width < @scroll
        @scroll = cursor_width - available + 1 if cursor_width >= @scroll + available
        @scroll = [[@scroll, 0].max, [total_width - available, 0].max].min
      end

      visible = slice_ansi(@highlighter.highlight(display), @scroll, available)
      ghost = suggestion_text(available, cursor_width)
      column = prompt_width + (cursor_width - @scroll) + 1
      column = [[column, 1].max, columns].min

      if @drawn_prompt_height.to_i.positive?
        @output.write("\e[#{@drawn_prompt_height - 1}A") if @drawn_prompt_height > 1
      end
      @output.write("\r")
      prompt_lines.each_with_index do |line, index|
        @output.write("\e[2K#{line}")
        @output.write("\r\n") if index < prompt_lines.length - 1
      end
      @output.write("#{visible}#{ghost}\e[J\e[#{column}G\e[?25h")
      @output.flush if @output.respond_to?(:flush)
      @drawn_prompt_height = prompt_lines.length
    end

    def prompt_text
      @prompt.respond_to?(:call) ? @prompt.call.to_s : @prompt.to_s
    end

    def display_text
      @buffer.text.gsub("\n", NEWLINE_GLYPH)
    end

    def strip_ansi(text)
      text.gsub(/\e\[[0-9;?]*[ -\/]*[@-~]/, "")
    end

    def slice_display(text, start, width)
      result = +""
      column = 0
      text.each_char do |char|
        char_width = @terminal.width(char)
        if column + char_width <= start
          column += char_width
          next
        end
        break if column >= start + width
        next if column < start
        break if column + char_width > start + width

        result << char
        column += char_width
      end
      result
    end

    def slice_ansi(text, start, width)
      result = +""
      pending = +""
      column = 0

      text.scan(/\e\[[0-9;?]*[ -\/]*[@-~]|[^\e]/) do |token|
        if token.start_with?("\e")
          pending << token
          next
        end

        char_width = @terminal.width(token)
        if column + char_width <= start
          column += char_width
          next
        end
        break if column >= start + width
        next if column < start
        break if column + char_width > start + width

        result << pending << token
        pending.clear
        column += char_width
      end

      result << "\e[0m" if result.include?("\e[")
      result
    end

    def suggestion
      return nil unless @history

      text = @buffer.text
      return nil if text.empty?
      return nil unless @buffer.cursor == text.length

      key = [text, @history.entries.length]
      return @suggestion if @suggestion_key == key

      @suggestion_key = key
      @suggestion = @history.search(text).find { |entry| entry.length > text.length }
    end

    def suggestion_text(available, cursor_width)
      text = suggestion
      return "" unless text

      remaining = available - (cursor_width - @scroll)
      return "" unless remaining.positive?

      suffix = strip_ansi(text[@buffer.text.length..].to_s).gsub("\n", NEWLINE_GLYPH)
      ghost = slice_display(suffix, 0, remaining)
      ghost.empty? ? "" : @theme.style(:suggestion, ghost)
    end

    def accept_suggestion
      text = suggestion
      return :handled unless text

      reset_history
      @buffer.replace(text)
      :handled
    end

    def accept_suggestion_word
      text = suggestion
      return edit { @buffer.word_right } unless text

      match = /\A\s*\S+/.match(text[@buffer.text.length..].to_s)
      insert(match[0]) if match
      :handled
    end

    def alt_right?(key)
      (key.type == :alt && key.value == :right) ||
        (key.type == :unknown && key.value == "\e[1;3C")
    end

    def command_exists(name)
      name = name.to_s
      return false if name.empty?

      @command_cache.fetch(name) do
        @command_cache[name] = command_known?(name)
      end
    end

    def command_known?(name)
      return true if Builtins.lookup(name)

      if @runtime
        return true if @runtime.aliases.key?(name)
        return true if @runtime.abbreviations.key?(name)
        return true if @runtime.commands.key?(name)
      end

      executable_path?(name)
    end

    def executable_path?(name)
      return File.file?(name) && File.executable?(name) if name.include?("/")

      ENV["PATH"].to_s.split(File::PATH_SEPARATOR).any? do |directory|
        next false if directory.empty?

        path = File.join(directory, name)
        File.file?(path) && File.executable?(path)
      end
    end
  end
end
