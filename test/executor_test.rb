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

  def test_expansion_failure_does_not_truncate_redirected_file
    path = File.join(@tmp, "important.txt")
    File.write(path, "keep this data\n")

    assert_equal 1, @shell.run('set -u; printf "$MISSING" > important.txt')
    assert_match(/MISSING: unbound variable/, @shell.stderr.string)
    assert_equal "keep this data\n", File.read(path)
  end

  def test_assignment_expansion_failure_does_not_truncate_redirected_file
    path = File.join(@tmp, "important.txt")
    File.write(path, "keep this data\n")

    assert_equal 1, @shell.run('set -u; VALUE="$MISSING" printf ok > important.txt')
    assert_match(/MISSING: unbound variable/, @shell.stderr.string)
    assert_equal "keep this data\n", File.read(path)
  end

  def test_redirect_target_expansion_failure_preserves_existing_output
    path = File.join(@tmp, "important.txt")
    File.write(path, "keep this data\n")

    assert_equal 1, @shell.run('set -u; printf ok > important.txt > "$MISSING"')
    assert_match(/MISSING: unbound variable/, @shell.stderr.string)
    assert_equal "keep this data\n", File.read(path)
  end

  def test_empty_program_returns_zero
    assert_equal 0, Sharky::Executor.new(@shell).execute(Sharky::AST::Program.new(lines: []))
  end
end
