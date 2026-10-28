require_relative "test_helper"
require "sharky"

class ASTTest < Minitest::Test
  def test_if_node
    first_condition = Sharky::AST::List.new(entries: [])
    first_body = Sharky::AST::List.new(entries: [])
    second_condition = Sharky::AST::List.new(entries: [])
    second_body = Sharky::AST::List.new(entries: [])
    else_body = Sharky::AST::List.new(entries: [])

    node = Sharky::AST::If.new(
      branches: [[first_condition, first_body], [second_condition, second_body]],
      else_body: else_body,
      pos: 10
    )

    assert_equal [[first_condition, first_body], [second_condition, second_body]], node.branches
    assert_same else_body, node.else_body
    assert_equal 10, node.pos
  end

  def test_for_node
    words = [Sharky::AST::Word.new(parts: [])]
    body = Sharky::AST::List.new(entries: [])

    node = Sharky::AST::For.new(name: "item", words: words, body: body, pos: 20)

    assert_equal "item", node.name
    assert_same words, node.words
    assert_same body, node.body
    assert_equal 20, node.pos
  end

  def test_while_node
    condition = Sharky::AST::List.new(entries: [])
    body = Sharky::AST::List.new(entries: [])

    node = Sharky::AST::While.new(condition: condition, body: body, until_form: true, pos: 30)

    assert_same condition, node.condition
    assert_same body, node.body
    assert node.until_form
    assert_equal 30, node.pos
  end

  def test_case_node
    word = Sharky::AST::Word.new(parts: [])
    first_patterns = [Sharky::AST::Word.new(parts: [])]
    first_body = Sharky::AST::List.new(entries: [])
    second_patterns = [Sharky::AST::Word.new(parts: []), Sharky::AST::Word.new(parts: [])]
    second_body = Sharky::AST::List.new(entries: [])

    node = Sharky::AST::Case.new(
      word: word,
      arms: [[first_patterns, first_body], [second_patterns, second_body]],
      pos: 40
    )

    assert_same word, node.word
    assert_equal [[first_patterns, first_body], [second_patterns, second_body]], node.arms
    assert_equal 40, node.pos
  end

  def test_function_def_node
    body = Sharky::AST::List.new(entries: [])

    node = Sharky::AST::FunctionDef.new(name: "greet", body: body, pos: 50)

    assert_equal "greet", node.name
    assert_same body, node.body
    assert_equal 50, node.pos
  end

  def test_group_node
    list = Sharky::AST::List.new(entries: [])

    node = Sharky::AST::Group.new(list: list, pos: 60)

    assert_same list, node.list
    assert_equal 60, node.pos
  end

  def test_arithmetic_node
    node = Sharky::AST::Arithmetic.new(source: "1 + 2", pos: 70)

    assert_equal "1 + 2", node.source
    assert_equal 70, node.pos
  end

  def test_test_command_node
    node = Sharky::AST::TestCommand.new(source: "-f file", double_bracket: true, pos: 80)

    assert_equal "-f file", node.source
    assert node.double_bracket
    assert_equal 80, node.pos
  end

  def test_heredoc_node
    node = Sharky::AST::HereDoc.new(
      delimiter: "EOF",
      strip_tabs: true,
      quoted: false,
      body: "hello\n",
      pos: 90
    )

    assert_equal "EOF", node.delimiter
    assert node.strip_tabs
    refute node.quoted
    assert_equal "hello\n", node.body
    assert_equal 90, node.pos
  end

  def test_redirect_carries_heredoc_and_here_string_targets
    heredoc = Sharky::AST::HereDoc.new(
      delimiter: "EOF", strip_tabs: false, quoted: true, body: "hi\n", pos: 100
    )
    here_string = Sharky::AST::Word.new(parts: [])

    input = Sharky::AST::Redirect.new(fd: 0, op: :heredoc, target: heredoc)
    string = Sharky::AST::Redirect.new(fd: 0, op: :here_string, target: here_string)

    assert_same heredoc, input.target
    assert_same here_string, string.target
  end
end
