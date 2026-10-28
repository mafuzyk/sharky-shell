require_relative "test_helper"
require "sharky"

class ParserTest < Minitest::Test
  def parse(source, compound: false)
    return Sharky::Parser.parse(source) unless compound

    Sharky::Parser.parse(source, compound: true)
  end

  def compound_parse(source)
    parse(source, compound: true)
  end

  def first_command(program)
    program.lines.first.list.entries.first[1].first.commands.first
  end

  def first_list_command(list)
    list.entries.first[1].first.commands.first
  end

  def word_text(word)
    word.parts.map { |part| part.type == :double ? part.value.map(&:value).join : part.value }.join
  end

  def word_values(command)
    command.words.map { |word| word_text(word) }
  end

  def test_simple_command_with_assignment_and_redirect
    program = parse("FOO=bar echo hi > out")
    simple = program.lines.first.list.entries.first[1].first.commands.first
    assert_equal ["FOO"], simple.assignments.map(&:name)
    assert_equal ["echo", "hi"], simple.words.map { |w| w.parts.first.value }
    assert_equal :out, simple.redirects.first.op
  end

  def test_pipeline_and_and_or
    program = parse("a | b && c || d")
    and_or = program.lines.first.list.entries.first[1].first
    assert_equal 2, and_or.first.commands.size
    assert_equal [:and, :or], and_or.rest.map(&:first)
  end

  def test_list_separators
    program = parse("a; b & c")
    kinds = program.lines.first.list.entries.map(&:first)
    assert_equal [:seq, :async], kinds
  end

  def test_subshell_and_redirect_descriptor
    program = parse("(cd /tmp; pwd) 2>&1")
    simple = program.lines.first.list.entries.first[1].first.commands.first
    assert_instance_of Sharky::AST::Subshell, simple
    redirect = simple.list.entries.last[1].first.commands.first.redirects.first
    assert_equal [2, 1], [redirect.fd, redirect.target]
  end

  def test_command_substitution_is_parsed
    program = parse("echo $(date)")
    part = program.lines.first.list.entries.first[1].first.commands.first.words[1].parts.first
    assert_instance_of Sharky::AST::Program, part.value
  end

  def test_incomplete_input_is_flagged
    error = assert_raises(Sharky::ParseError) { parse("echo hi |") }
    assert error.incomplete?
  end

  def test_reserved_word_is_rejected
    error = assert_raises(Sharky::ParseError) { parse("for x in 1 2; do echo $x; done") }
    assert_includes error.message, "unsupported syntax"
    refute error.incomplete?
  end

  def test_compound_mode_parses_if
    node = first_command(compound_parse("if true; then echo yes; fi"))
    assert_instance_of Sharky::AST::If, node
    assert_equal 0, node.pos
    assert_nil node.else_body
    assert_equal 1, node.branches.size

    condition, body = node.branches.first
    assert_equal ["true"], word_values(first_list_command(condition))
    assert_equal ["echo", "yes"], word_values(first_list_command(body))
  end

  def test_default_mode_still_rejects_compound_if
    error = assert_raises(Sharky::ParseError) { parse("if true; then echo yes; fi") }
    assert_equal "unsupported syntax: 'if'. Use bash -c '...' or a Ruby function.", error.message
    refute error.incomplete?
  end

  def test_compound_if_parses_elif_and_else
    node = first_command(compound_parse("if a; then b; elif c; then d; else e; fi"))

    assert_equal 2, node.branches.size
    assert_equal ["a"], word_values(first_list_command(node.branches[0][0]))
    assert_equal ["b"], word_values(first_list_command(node.branches[0][1]))
    assert_equal ["c"], word_values(first_list_command(node.branches[1][0]))
    assert_equal ["d"], word_values(first_list_command(node.branches[1][1]))
    assert_equal ["e"], word_values(first_list_command(node.else_body))
  end

  def test_compound_if_parses_newline_separators
    source = "if true\nthen\n  echo yes\nelse\n  echo no\nfi"
    node = first_command(compound_parse(source))

    assert_equal 1, compound_parse(source).lines.size
    assert_equal ["echo", "yes"], word_values(first_list_command(node.branches[0][1]))
    assert_equal ["echo", "no"], word_values(first_list_command(node.else_body))
  end

  def test_compound_if_parses_nested_if
    source = "if true; then if false; then echo no; else echo yes; fi; fi"
    outer = first_command(compound_parse(source))
    inner = first_list_command(outer.branches[0][1])

    assert_instance_of Sharky::AST::If, inner
    assert_equal source.index("if", 1), inner.pos
    assert_equal ["echo", "yes"], word_values(first_list_command(inner.else_body))
  end

  def test_compound_for_parses_words_and_body
    node = first_command(compound_parse("for x in 1 2 3; do echo $x; done"))

    assert_instance_of Sharky::AST::For, node
    assert_equal "x", node.name
    assert_equal ["1", "2", "3"], node.words.map { |word| word.parts.first.value }
    assert_equal 0, node.pos
    assert_equal ["echo", "x"], word_values(first_list_command(node.body))
  end

  def test_compound_for_without_in_list
    node = first_command(compound_parse("for x; do echo $x; done"))

    assert_instance_of Sharky::AST::For, node
    assert_equal [], node.words
    assert_equal ["echo", "x"], word_values(first_list_command(node.body))
  end

  def test_compound_for_treats_do_in_word_list_as_a_word
    node = first_command(compound_parse("for x in do; do echo $x; done"))

    assert_equal ["do"], node.words.map { |word| word.parts.first.value }
    assert_equal ["echo", "x"], word_values(first_list_command(node.body))
  end

  def test_compound_for_allows_newline_before_in
    node = first_command(compound_parse("for x\nin 1 2; do echo $x; done"))

    assert_equal ["1", "2"], node.words.map { |word| word.parts.first.value }
    assert_equal ["echo", "x"], word_values(first_list_command(node.body))
  end

  def test_compound_while_parses_newline_separators
    source = "while true\ndo\n  echo hi\ndone"
    node = first_command(compound_parse(source))

    assert_instance_of Sharky::AST::While, node
    refute node.until_form
    assert_equal 0, node.pos
    assert_equal ["true"], word_values(first_list_command(node.condition))
    assert_equal ["echo", "hi"], word_values(first_list_command(node.body))
  end

  def test_compound_until_parses_condition_and_body
    node = first_command(compound_parse("until false; do echo hi; done"))

    assert_instance_of Sharky::AST::While, node
    assert node.until_form
    assert_equal ["false"], word_values(first_list_command(node.condition))
    assert_equal ["echo", "hi"], word_values(first_list_command(node.body))
  end

  def test_compound_if_without_fi_is_incomplete
    error = assert_raises(Sharky::ParseError) { compound_parse("if true; then echo yes;") }
    assert error.incomplete?
  end

  def test_compound_for_without_done_is_incomplete
    error = assert_raises(Sharky::ParseError) { compound_parse("for x in 1 2; do echo $x") }
    assert error.incomplete?
  end

  def test_compound_while_without_done_is_incomplete
    error = assert_raises(Sharky::ParseError) { compound_parse("while true; do echo hi") }
    assert error.incomplete?
  end

  def test_compound_for_inside_if_has_source_position
    source = "if true; then for x in 1; do echo $x; done; fi"
    outer = first_command(compound_parse(source))
    node = first_list_command(outer.branches[0][1])

    assert_instance_of Sharky::AST::For, node
    assert_equal source.index("for"), node.pos
  end

  def test_compound_command_can_be_followed_by_more_commands
    semicolon = compound_parse("if true; then echo yes; fi; echo after")
    newline = compound_parse("if true; then echo yes; fi\necho after")

    assert_equal 2, semicolon.lines.first.list.entries.size
    assert_equal 2, newline.lines.size
  end

  def test_compound_case_parses_arms_with_multiple_patterns
    source = "case fruit in apple|p?ar) echo fruit;; *) echo other;; esac"
    node = first_command(compound_parse(source))

    assert_instance_of Sharky::AST::Case, node
    assert_equal "fruit", word_text(node.word)
    assert_equal 0, node.pos
    assert_equal 2, node.arms.size

    patterns, body = node.arms.first
    assert_equal %w[apple p?ar], patterns.map { |pattern| word_text(pattern) }
    assert_equal ["echo", "fruit"], word_values(first_list_command(body))

    patterns, body = node.arms.last
    assert_equal ["*"], patterns.map { |pattern| word_text(pattern) }
    assert_equal ["echo", "other"], word_values(first_list_command(body))
  end

  def test_compound_case_parses_quoted_pattern_and_final_arm_without_terminator
    source = "case $x in\n  \"a b\") echo spaced;;\n  *) echo other\nesac"
    node = first_command(compound_parse(source))

    assert_equal 2, node.arms.size
    patterns, body = node.arms.first
    assert_equal ["a b"], patterns.map { |pattern| word_text(pattern) }
    assert_equal ["echo", "spaced"], word_values(first_list_command(body))
    assert_equal ["echo", "other"], word_values(first_list_command(node.arms.last[1]))
  end

  def test_compound_case_parses_nested_body
    source = "case $x in a) if true; then echo yes; fi;; esac"
    node = first_command(compound_parse(source))
    inner = first_list_command(node.arms.first[1])

    assert_instance_of Sharky::AST::If, inner
    assert_equal source.index("if"), inner.pos
    assert_equal ["echo", "yes"], word_values(first_list_command(inner.branches.first[1]))
  end

  def test_compound_case_without_esac_is_incomplete
    error = assert_raises(Sharky::ParseError) { compound_parse("case x in a) echo hi;;") }
    assert error.incomplete?
  end

  def test_compound_function_definition_parses_short_form
    source = "greet() { if true; then echo hi; fi; }"
    node = first_command(compound_parse(source))
    inner = first_list_command(node.body)

    assert_instance_of Sharky::AST::FunctionDef, node
    assert_equal "greet", node.name
    assert_equal 0, node.pos
    assert_instance_of Sharky::AST::If, inner
    assert_equal source.index("if"), inner.pos
    assert_equal ["echo", "hi"], word_values(first_list_command(inner.branches.first[1]))
  end

  def test_compound_function_definition_parses_keyword_form
    node = first_command(compound_parse("function greet { echo hello; }"))

    assert_instance_of Sharky::AST::FunctionDef, node
    assert_equal "greet", node.name
    assert_equal 0, node.pos
    assert_equal ["echo", "hello"], word_values(first_list_command(node.body))
  end

  def test_compound_function_without_body_is_incomplete
    error = assert_raises(Sharky::ParseError) { compound_parse("function greet") }
    assert error.incomplete?
  end

  def test_compound_function_without_closing_brace_is_incomplete
    error = assert_raises(Sharky::ParseError) { compound_parse("greet() { echo hi;") }
    assert error.incomplete?
  end

  def test_compound_group_parses_body
    node = first_command(compound_parse("{ echo hi; }"))

    assert_instance_of Sharky::AST::Group, node
    assert_equal 0, node.pos
    assert_equal ["echo", "hi"], word_values(first_list_command(node.list))
  end

  def test_compound_group_parses_nested_group
    source = "{ { echo inner; }; echo outer; }"
    outer = first_command(compound_parse(source))
    inner = first_list_command(outer.list)

    assert_instance_of Sharky::AST::Group, outer
    assert_equal 2, outer.list.entries.size
    assert_instance_of Sharky::AST::Group, inner
    assert_equal source.index("{", 1), inner.pos
    assert_equal ["echo", "inner"], word_values(first_list_command(inner.list))
  end

  def test_compound_group_without_closing_brace_is_incomplete
    error = assert_raises(Sharky::ParseError) { compound_parse("{ echo hi;") }
    assert error.incomplete?
  end

  def test_default_mode_still_rejects_case_and_function
    case_error = assert_raises(Sharky::ParseError) { parse("case x in a) echo hi;; esac") }
    function_error = assert_raises(Sharky::ParseError) { parse("function f { echo hi; }") }

    assert_equal "unsupported syntax: 'case'. Use bash -c '...' or a Ruby function.", case_error.message
    assert_equal "unsupported syntax: 'function'. Use bash -c '...' or a Ruby function.",
                 function_error.message
  end
end
