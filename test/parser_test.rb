require_relative "test_helper"
require "sharky"

class ParserTest < Minitest::Test
  def parse(source)
    Sharky::Parser.parse(source)
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
end
