# frozen_string_literal: true

require_relative "test_helper"
require "sharky"

class KeysTest < Minitest::Test
  def test_decodes_arrow_ctrl_and_enter
    keys = Sharky::Keys.decode("\e[A\x03\r")
    assert_equal [:up, :ctrl, :enter], keys.map(&:type)
    assert_equal "c", keys[1].value
  end

  def test_decodes_alt_and_bracketed_paste
    keys = Sharky::Keys.decode("\ef\e[200~hi\e[201~")
    assert_equal [:alt, :paste_start, :char, :char, :paste_end], keys.map(&:type)
    assert_equal "f", keys[0].value
    assert_equal "h", keys[2].value
  end

  def test_decodes_named_keys
    keys = Sharky::Keys.decode("\e[D\e[C\e[H\e[F\e[3~\x7f\t\e")
    assert_equal [:left, :right, :home, :end, :delete, :backspace, :tab, :escape], keys.map(&:type)
  end

  def test_decodes_utf8_characters
    keys = Sharky::Keys.decode("é日")
    assert_equal [:char, :char], keys.map(&:type)
    assert_equal ["é", "日"], keys.map(&:value)
  end

  def test_decoder_waits_for_split_escape_sequence
    decoder = Sharky::Keys::Decoder.new
    assert_empty decoder.feed("\e[")
    keys = decoder.feed("A")
    assert_equal [:up], keys.map(&:type)
  end

  def test_unknown_bytes_are_preserved
    keys = Sharky::Keys.decode("\x01\xff")
    assert_equal [:ctrl, :unknown], keys.map(&:type)
    assert_equal "a", keys[0].value
  end
end
