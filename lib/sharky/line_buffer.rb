# frozen_string_literal: true

module Sharky
  class LineBuffer
    UNDO_LIMIT = 200

    attr_reader :text, :cursor

    def initialize(text = "")
      @text = text.to_s.dup
      @cursor = @text.length
      @undo = []
      @redo = []
      @kill = ""
    end

    def insert(value)
      value = value.to_s
      return self if value.empty?

      record
      @text = @text[0...@cursor] + value + @text[@cursor..].to_s
      @cursor += value.length
      self
    end

    def backspace
      return self if @cursor.zero?

      record
      @text = @text[0...(@cursor - 1)] + @text[@cursor..].to_s
      @cursor -= 1
      self
    end

    def delete
      return self if @cursor >= @text.length

      record
      @text = @text[0...@cursor] + @text[(@cursor + 1)..].to_s
      self
    end

    def left
      @cursor -= 1 if @cursor.positive?
      self
    end

    def right
      @cursor += 1 if @cursor < @text.length
      self
    end

    def home
      @cursor = 0
      self
    end

    def end
      @cursor = @text.length
      self
    end

    def kill_to_end
      return self if @cursor >= @text.length

      record
      @kill = @text[@cursor..].to_s
      @text = @text[0...@cursor]
      self
    end

    def kill_to_start
      return self if @cursor.zero?

      record
      @kill = @text[0...@cursor]
      @text = @text[@cursor..].to_s
      @cursor = 0
      self
    end

    def kill_word
      return self if @cursor.zero?

      start = @cursor
      start -= 1 while start.positive? && @text[start - 1].match?(/\s/)
      start -= 1 while start.positive? && !@text[start - 1].match?(/\s/)
      return self if start == @cursor

      record
      @kill = @text[start...@cursor]
      @text = @text[0...start] + @text[@cursor..].to_s
      @cursor = start
      self
    end

    def yank
      insert(@kill) unless @kill.empty?
      self
    end

    def word_left
      index = @cursor
      index -= 1 while index.positive? && @text[index - 1].match?(/\s/)
      index -= 1 while index.positive? && !@text[index - 1].match?(/\s/)
      @cursor = index
      self
    end

    def word_right
      index = @cursor
      index += 1 while index < @text.length && !@text[index].match?(/\s/)
      index += 1 while index < @text.length && @text[index].match?(/\s/)
      @cursor = index
      self
    end

    def replace(value)
      record
      @text = value.to_s.dup
      @cursor = @text.length
      self
    end

    def clear
      replace("")
    end

    def empty?
      @text.empty?
    end

    def undo
      state = @undo.pop
      return self unless state

      @redo << [@text.dup, @cursor]
      restore(state)
      self
    end

    def redo
      state = @redo.pop
      return self unless state

      @undo << [@text.dup, @cursor]
      restore(state)
      self
    end

    def to_s
      @text
    end

    private

    def record
      @undo << [@text.dup, @cursor]
      @undo.shift if @undo.length > UNDO_LIMIT
      @redo.clear
    end

    def restore(state)
      @text = state[0]
      @cursor = state[1]
    end
  end
end
