require_relative "test_helper"
require "sharky"

class LexerTest < Minitest::Test
  def lex(source)
    Sharky::Lexer.new(source).tokenize
  end

  def test_words_and_pipe
    tokens = lex("echo hi | wc -l")
    assert_equal [:word, :word, :pipe, :word, :word, :eof], tokens.map(&:type)
    assert_equal "echo", tokens[0].value.parts.first.value
  end

  def test_quotes_and_variables
    tokens = lex(%q{echo "a $HOME" 'b c' $PATH ${PATH}})
    assert_equal [:word, :word, :word, :word, :word, :eof], tokens.map(&:type)
    assert_equal :double, tokens[1].value.parts.first.type
    assert_equal :single, tokens[2].value.parts.first.type
    assert_equal :variable, tokens[3].value.parts.first.type
    assert_equal :variable, tokens[4].value.parts.first.type
  end

  def test_command_substitution_and_nesting
    tokens = lex("echo $(printf '%s' $(date))")
    part = tokens[1].value.parts.first
    assert_equal :cmdsub, part.type
    assert_equal "printf '%s' $(date)", part.value
  end

  def test_operators_and_redirects
    tokens = lex("a && b || c; d & e > f 2>&1 &> g")
    assert_equal(
      [:word, :and_if, :word, :or_if, :word, :semi, :word, :amp, :word, :redir_out, :word, :redir_dup, :redir_both, :word, :eof],
      tokens.map(&:type)
    )
  end

  def test_comment_stops_at_newline
    tokens = lex("echo hi # comment\n")
    assert_equal [:word, :word, :newline, :eof], tokens.map(&:type)
  end

  def test_unclosed_quote_is_incomplete
    error = assert_raises(Sharky::LexError) { lex("echo 'x") }
    assert error.incomplete?
  end
end
