# frozen_string_literal: true

require_relative "test_helper"
require "sharky"

class LineBufferTest < Minitest::Test
  def test_editing_moves_cursor_and_supports_undo
    buffer = Sharky::LineBuffer.new("hello")
    buffer.left
    buffer.left
    buffer.insert("X")
    assert_equal "helXlo", buffer.text
    buffer.backspace
    assert_equal "hello", buffer.text
    buffer.home
    buffer.kill_to_end
    assert_equal "", buffer.text
    buffer.undo
    assert_equal "hello", buffer.text
  end

  def test_embedded_newlines_are_preserved
    buffer = Sharky::LineBuffer.new("a")
    buffer.insert("\nb")
    assert_equal "a\nb", buffer.text
    assert_equal 3, buffer.cursor
  end

  def test_delete_and_word_operations
    buffer = Sharky::LineBuffer.new("one two three")
    buffer.word_left
    assert_equal 8, buffer.cursor
    buffer.kill_word
    assert_equal "one three", buffer.text
    buffer.yank
    assert_equal "one two three", buffer.text
    buffer.home
    buffer.delete
    assert_equal "ne two three", buffer.text
  end

  def test_undo_and_redo
    buffer = Sharky::LineBuffer.new("a")
    buffer.insert("b")
    assert_equal "ab", buffer.text
    buffer.undo
    assert_equal "a", buffer.text
    buffer.redo
    assert_equal "ab", buffer.text
  end
end
