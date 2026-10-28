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

  def parameter(name, operator: nil, argument: nil)
    Sharky::AST::Part.new(type: :parameter, value: Sharky::AST::Parameter.new(name: name, operator: operator, argument: argument))
  end

  def test_braced_parameter_plain_and_default
    assert_equal ["value"], expansion(env: { "X" => "value" }).expand_word(word(parameter("X")), quoted: true)
    assert_equal ["fallback"], expansion(env: {}).expand_word(word(parameter("X", operator: :default, argument: "fallback")), quoted: true)
    assert_equal ["value"], expansion(env: { "X" => "value" }).expand_word(word(parameter("X", operator: :default, argument: "fallback")), quoted: true)
  end

  def test_braced_parameter_assign_and_alternate
    env = {}
    exp = expansion(env: env)
    assert_equal ["assigned"], exp.expand_word(word(parameter("X", operator: :assign, argument: "assigned")), quoted: true)
    assert_equal "assigned", env["X"]
    assert_equal ["alt"], expansion(env: { "X" => "yes" }).expand_word(word(parameter("X", operator: :alternate, argument: "alt")), quoted: true)
    assert_equal [], expansion(env: {}).expand_word(word(parameter("X", operator: :alternate, argument: "alt")))
  end

  def test_braced_parameter_length_and_trim
    assert_equal ["3"], expansion(env: { "X" => "abc" }).expand_word(word(parameter("X", operator: :length)), quoted: true)
    assert_equal ["bar.txt"], expansion(env: { "X" => "foo.bar.txt" }).expand_word(word(parameter("X", operator: :trim_prefix_short, argument: "*.")), quoted: true)
    assert_equal ["txt"], expansion(env: { "X" => "foo.bar.txt" }).expand_word(word(parameter("X", operator: :trim_prefix_long, argument: "*.")), quoted: true)
    assert_equal ["foo.bar"], expansion(env: { "X" => "foo.bar.txt" }).expand_word(word(parameter("X", operator: :trim_suffix_short, argument: ".*")), quoted: true)
    assert_equal ["foo"], expansion(env: { "X" => "foo.bar.txt" }).expand_word(word(parameter("X", operator: :trim_suffix_long, argument: ".*")), quoted: true)
  end

  def test_braced_parameter_substitution
    assert_equal ["bar.bar.txt"], expansion(env: { "X" => "foo.bar.txt" }).expand_word(word(parameter("X", operator: :substitute, argument: "foo/bar")), quoted: true)
    assert_equal ["bar.bar.txt"], expansion(env: { "X" => "foo.foo.txt" }).expand_word(word(parameter("X", operator: :substitute_all, argument: "foo/bar")), quoted: true)
  end

  def test_braced_parameter_error_is_raised
    assert_raises(Sharky::Expansion::ParameterError) do
      expansion(env: {}).expand_word(word(parameter("X", operator: :error, argument: "missing")))
    end
  end

  def test_braced_parameter_quoted_empty_and_splitting
    assert_equal [""], expansion(env: {}).expand_word(word(parameter("X", operator: :default, argument: "")), quoted: true)
    assert_equal [], expansion(env: {}).expand_word(word(parameter("X", operator: :default, argument: "")))
    assert_equal ["a", "b"], expansion(env: { "X" => "a b" }).expand_word(word(parameter("X")))
  end

  def test_arithmetic_expansion
    part = Sharky::AST::Part.new(
      type: :arithmetic,
      value: Sharky::AST::ArithmeticExpansion.new(source: "1 + 2 * 3")
    )

    assert_equal ["7"], expansion.expand_word(word(part), quoted: true)
    assert_equal ["7"], expansion.expand_word(word(part))
  end

  def test_brace_alternatives_and_nesting
    assert_equal ["abd", "acd"], expansion.expand_word(word(lit("a{b,c}d")))
    assert_equal ["abe", "ace", "ade"], expansion.expand_word(word(lit("a{b,{c,d}}e")))
    assert_equal ["a", "ab", "ac"], expansion.expand_word(word(lit("a{,b,c}")))
  end

  def test_brace_ranges
    assert_equal %w[1 2 3], expansion.expand_word(word(lit("{1..3}")))
    assert_equal %w[3 2 1], expansion.expand_word(word(lit("{3..1}")))
    assert_equal %w[a b c], expansion.expand_word(word(lit("{a..c}")))
    assert_equal %w[1 3 5], expansion.expand_word(word(lit("{1..5..2}")))
  end

  def test_quoted_braces_do_not_expand
    part = Sharky::AST::Part.new(type: :single, value: "a{b,c}")
    assert_equal ["a{b,c}"], expansion.expand_word(word(part))
    double = Sharky::AST::Part.new(type: :double, value: [lit("a{b,c}")])
    assert_equal ["a{b,c}"], expansion.expand_word(word(double))
  end

  def test_malformed_braces_stay_literal
    assert_equal ["a{b,c"], expansion.expand_word(word(lit("a{b,c")))
    assert_equal ["{a}"], expansion.expand_word(word(lit("{a}")))
  end
end
