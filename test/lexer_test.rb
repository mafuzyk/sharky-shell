require_relative "test_helper"
require "sharky"

class LexerTest < Minitest::Test
  def lex(source)
    Sharky::Lexer.new(source).tokenize
  end

  def compound_lex(source)
    Sharky::Lexer.new(source, compound: true).tokenize
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

  def test_compound_bang_delimiter
    assert_equal [:bang, :word, :eof], compound_lex("! true").map(&:type)
    assert_equal [:word, :bang, :eof], compound_lex("true !").map(&:type)
  end

  def test_bang_stays_literal_inside_words
    tokens = compound_lex("echo hi! !ready")
    assert_equal [:word, :word, :word, :eof], tokens.map(&:type)
    assert_equal %w[echo hi! !ready], tokens.first(3).map { |token| token.value.parts.map(&:value).join }
  end

  def test_compound_group_delimiters
    tokens = compound_lex("{\nprintf hi\n}")
    assert_equal [:lbrace, :newline, :word, :word, :newline, :rbrace, :eof], tokens.map(&:type)
    assert_equal [:lbrace, :word, :semi, :rbrace, :eof], compound_lex("{ echo; }").map(&:type)
  end

  def test_braces_stay_literal_inside_words
    assert_equal [:word, :word, :word, :eof], compound_lex("echo {a,b} a}b").map(&:type)
  end

  def test_compound_arithmetic_delimiters
    assert_equal [:arith_start, :word, :word, :word, :arith_end, :eof], compound_lex("(( x + 1 ))").map(&:type)
    assert_equal [:arith_start, :word, :arith_end, :eof], compound_lex("((x))").map(&:type)
  end

  def test_compound_test_delimiters
    assert_equal [:test_start, :word, :word, :test_end, :eof], compound_lex("[[ -n x ]]").map(&:type)
    assert_equal [:test_start, :word, :test_end, :eof], compound_lex("[[x]]").map(&:type)
  end

  def test_single_brackets_stay_words
    assert_equal [:word, :word, :word, :word, :eof], compound_lex("[ -f x ]").map(&:type)
  end

  def test_double_paren_in_command_substitution_is_still_rejected
    error = assert_raises(Sharky::LexError) { compound_lex("echo $((1 + 2))") }
    assert_includes error.message, "unsupported syntax: '$(('"
  end

  def test_compound_here_doc_and_here_string
    expected = [:word, :heredoc, :word, :heredoc_strip, :word, :here_string, :word, :eof]
    assert_equal expected, compound_lex("cat <<EOF <<-END <<<word").map(&:type)
    assert_equal expected, compound_lex("cat << EOF <<- END <<< word").map(&:type)
  end

  def test_compound_append_both_redirect
    expected = [:word, :redir_both_append, :word, :eof]
    assert_equal expected, compound_lex("cmd &>> log").map(&:type)
    assert_equal expected, compound_lex("cmd&>>log").map(&:type)
    assert_equal [:word, :redir_both, :word, :eof], compound_lex("cmd &> log").map(&:type)
  end

  def test_compound_case_terminators
    expected = [:word, :case_break, :word, :case_continue, :word, :eof]
    assert_equal expected, compound_lex("a ;& b ;;& c").map(&:type)
    assert_equal expected, compound_lex("a;&b;;&c").map(&:type)
  end

  def test_case_end_remains_two_semicolons
    assert_equal [:word, :semi, :semi, :word, :eof], compound_lex("a ;; b").map(&:type)
  end

  def test_reserved_words_stay_words
    words = %w[for while until if then elif else fi case esac function select time coproc]
    tokens = compound_lex(words.join(" "))
    assert_equal Array.new(words.length, :word) + [:eof], tokens.map(&:type)
    assert_equal words, tokens.first(words.length).map { |token| token.value.parts.map(&:value).join }
  end

  def test_default_mode_keeps_legacy_compound_tokens
    assert_equal [:word, :word, :word, :word, :eof], lex("[[ -n x ]]").map(&:type)
    assert_equal [:lparen, :lparen, :word, :rparen, :rparen, :eof], lex("((x))").map(&:type)
    assert_equal [:word, :redir_in, :redir_in, :word, :eof], lex("cat <<EOF").map(&:type)
    assert_equal [:word, :redir_both, :redir_out, :word, :eof], lex("cmd &>> log").map(&:type)
    assert_equal [:word, :semi, :amp, :word, :semi, :semi, :amp, :word, :eof], lex("a ;& b ;;& c").map(&:type)
  end
end
