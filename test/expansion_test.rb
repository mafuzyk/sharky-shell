require_relative "test_helper"
require "sharky"

class ExpansionTest < Minitest::Test
  def word(*parts)
    Sharky::AST::Word.new(parts: parts)
  end

  def lit(text)
    Sharky::AST::Part.new(type: :literal, value: text)
  end

  def expansion(env: {}, command_substitution: ->(_program) { "" })
    Sharky::Expansion.new(env: env, cwd: @tmp, command_substitution: command_substitution)
  end

  def test_quoted_empty_stays_and_unquoted_empty_disappears
    env = { "EMPTY" => "" }
    assert_equal [""], expansion(env: env).expand_word(word(Sharky::AST::Part.new(type: :variable, value: "EMPTY")), quoted: true)
    assert_equal [], expansion(env: env).expand_word(word(Sharky::AST::Part.new(type: :variable, value: "EMPTY")))
  end

  def test_field_splitting_only_for_unquoted_expansion
    env = { "X" => "a b" }
    part = Sharky::AST::Part.new(type: :variable, value: "X")
    assert_equal ["a", "b"], expansion(env: env).expand_word(word(part))
    double = Sharky::AST::Part.new(type: :double, value: [part])
    assert_equal ["a b"], expansion(env: env).expand_word(word(double))
  end

  def test_globbing_and_no_match_literal
    File.write(File.join(@tmp, "a.txt"), "")
    assert_equal [File.join(@tmp, "a.txt")], expansion.expand_word(word(lit(File.join(@tmp, "*.txt"))))
    assert_equal [File.join(@tmp, "*.nope")], expansion.expand_word(word(lit(File.join(@tmp, "*.nope"))))
  end

  def test_tilde_and_command_substitution
    prog = Sharky::AST::Program.new(lines: [])
    assert_equal [@tmp], expansion(env: { "HOME" => @tmp }).expand_word(word(Sharky::AST::Part.new(type: :tilde, value: "")))
    assert_equal ["x"], expansion(command_substitution: ->(_p) { "x\n" }).expand_word(
      word(Sharky::AST::Part.new(type: :cmdsub, value: prog)), quoted: true
    )
  end

  def test_assignment_is_not_split
    env = { "X" => "a b" }
    part = Sharky::AST::Part.new(type: :variable, value: "X")
    assert_equal "a b", expansion(env: env).expand_assignment(word(part))
  end
end
