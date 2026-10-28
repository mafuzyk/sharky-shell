require_relative "test_helper"
require "sharky"

class ArithmeticTest < Minitest::Test
  def evaluate(source, env: {})
    Sharky::Arithmetic.evaluate(source, env: env)
  end

  def test_precedence_and_parentheses
    assert_equal 7, evaluate("1 + 2 * 3")
    assert_equal 9, evaluate("(1 + 2) * 3")
  end

  def test_variables_and_unset
    assert_equal 4, evaluate("X + 1", env: { "X" => "3" })
    assert_equal 0, evaluate("MISSING")
    assert_equal 3, evaluate("$X", env: { "X" => "3" })
  end

  def test_comparisons_and_logic
    assert_equal 1, evaluate("2 > 1")
    assert_equal 0, evaluate("1 == 2")
    assert_equal 1, evaluate("1 == 1 && 2 > 1")
    assert_equal 1, evaluate("1 || 0")
    assert_equal 0, evaluate("!1")
  end

  def test_logical_or_short_circuits_without_skipping_syntax_checks
    assert_equal 1, evaluate("1 || 1 / 0")
    assert_equal 1, evaluate("1 || (1 && 1 / 0)")
    assert_raises(Sharky::Arithmetic::Error) { evaluate("(1 || 1 / 0) / 0") }
    assert_raises(Sharky::Arithmetic::Error) { evaluate("1 || (1 +)") }
  end

  def test_logical_and_short_circuits_nested_expressions
    assert_equal 0, evaluate("0 && 1 / 0")
    assert_equal 0, evaluate("0 && (0 || 1 % 0)")
    assert_raises(Sharky::Arithmetic::Error) { evaluate("0 && (1 +)") }
  end

  def test_integer_division_truncates_toward_zero
    assert_equal(-1, evaluate("-3 / 2"))
    assert_equal(-1, evaluate("3 / -2"))
    assert_equal 1, evaluate("-3 / -2")
    assert_equal 9007199254740993, evaluate("18014398509481987 / 2")
  end

  def test_integer_remainder_has_the_dividends_sign
    assert_equal(-1, evaluate("-3 % 2"))
    assert_equal 1, evaluate("3 % -2")
    assert_equal(-1, evaluate("-3 % -2"))
  end

  def test_relations_bind_tighter_than_equality
    assert_equal 0, evaluate("2 == 1 < 1")
    assert_equal 1, evaluate("2 != 1 < 1")
    assert_equal 1, evaluate("1 == 2 > 0")
  end

  def test_division_by_zero_and_malformed
    assert_raises(Sharky::Arithmetic::Error) { evaluate("1 / 0") }
    assert_raises(Sharky::Arithmetic::Error) { evaluate("1 +") }
  end
end
