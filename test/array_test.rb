require_relative "test_helper"
require "sharky"
require "stringio"

class ArrayTest < Minitest::Test
  def setup
    super
    @stdout = StringIO.new
    @stderr = StringIO.new
    @shell = Sharky::Shell.new(
      env: { "PATH" => "/usr/bin:/bin" },
      cwd: @tmp,
      stdin: StringIO.new,
      stdout: @stdout,
      stderr: @stderr
    )
  end

  def run_compound(source)
    @shell.run(source, compound: true)
  end

  def test_array_assignment_ast
    program = Sharky::Parser.parse("a=(one two)", compound: true)
    assignment = program.lines.first.list.entries.first[1].first.commands.first.assignments.first

    assert_equal "a", assignment.name
    assert_equal 2, assignment.array.length
  end

  def test_element_zero_and_index
    run_compound("a=(one two); printf '%s|%s' \"$a\" \"${a[1]}\"")
    assert_equal "one|two", @stdout.string
  end

  def test_length_append_and_replace
    run_compound("a=(one two); a+=(three); a[1]=changed; printf '%s|%s' \"${#a[@]}\" \"${a[@]}\"")
    assert_equal "3|one changed three", @stdout.string
  end

  def test_empty_and_unset_arrays
    run_compound("a=(); printf '%s|%s' \"${#a[@]}\" \"${b[@]}\"")
    assert_equal "0|", @stdout.string
  end

  def test_indexed_assignment_creates_element
    run_compound("a[0]=zero; printf '%s' \"${a[0]}\"")
    assert_equal "zero", @stdout.string
  end
end
