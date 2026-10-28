require_relative "test_helper"
require "sharky"
require "stringio"

class ExecutorTest < Minitest::Test
  def setup
    super
    @shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new, stdin: StringIO.new)
  end

  def test_executes_a_parsed_program
    program = Sharky::Parser.parse("true; false")
    assert_equal 1, Sharky::Executor.new(@shell).execute(program)
  end

  def test_empty_program_returns_zero
    assert_equal 0, Sharky::Executor.new(@shell).execute(Sharky::AST::Program.new(lines: []))
  end
end
