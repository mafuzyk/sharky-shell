require_relative "test_helper"
require "sharky"
require "stringio"

class FunctionsTest < Minitest::Test
  def test_autoloads_function_by_name
    functions = File.join(@tmp, "config", "sharky", "functions")
    FileUtils.mkdir_p(functions)
    File.write(File.join(functions, "hello.rb"), <<~'RUBY')
      Sharky.command :hello do |argv, stdin|
        puts "hello #{argv.join(' ')}"
        0
      end
    RUBY
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    runtime.config_dir = File.join(@tmp, "config", "sharky")
    shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: StringIO.new, stderr: StringIO.new)
    shell.runtime = runtime
    assert_equal 0, shell.run("hello world")
    assert_equal "hello world\n", shell.stdout.string
  end

  def test_function_exception_becomes_status_one
    Sharky::Runtime.new.tap { |r| Sharky.runtime = r }
    sharky = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp,
                               stdout: StringIO.new, stderr: StringIO.new)
    sharky.runtime = Sharky.runtime
    Sharky.command(:boom) { |_argv, _stdin| raise "bad" }
    assert_equal 1, sharky.run("boom")
    assert_includes sharky.stderr.string, "function boom failed"
  end

  def test_function_state_changes_affect_the_shell
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    Sharky.command(:change) do |_argv, _stdin|
      Dir.chdir("..")
      ENV["SHARKY_TEST_STATE"] = "yes"
      0
    end
    sharky = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp,
                               stdout: StringIO.new, stderr: StringIO.new)
    sharky.runtime = runtime

    assert_equal 0, sharky.run("change")
    assert_equal File.dirname(@tmp), sharky.cwd
    assert_equal "yes", sharky.env["SHARKY_TEST_STATE"]
  ensure
    ENV.delete("SHARKY_TEST_STATE")
  end

  def test_ruby_builtin_runs_repl_helpers
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    sharky = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp,
                               stdin: StringIO.new("sh 'printf repl'\nexit\n"),
                               stdout: StringIO.new, stderr: StringIO.new)
    sharky.runtime = runtime
    assert_equal 0, sharky.run("ruby")
    assert_includes sharky.stdout.string, "repl"
  end

  def test_ruby_repl_capture_and_cd_helpers
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    input = StringIO.new("print capture('printf cap')\ncd '..'\nputs Sharky.cwd\nexit\n")
    sharky = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp,
                               stdin: input, stdout: StringIO.new, stderr: StringIO.new)
    sharky.runtime = runtime

    assert_equal 0, sharky.run("ruby")
    assert_includes sharky.stdout.string, "cap"
    assert_includes sharky.stdout.string, File.dirname(@tmp)
  end

  def test_ruby_repl_state_changes_affect_the_shell
    runtime = Sharky::Runtime.new
    Sharky.runtime = runtime
    input = StringIO.new("ENV['SHARKY_REPL_STATE'] = 'yes'\nDir.chdir('..')\nexit\n")
    sharky = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp,
                               stdin: input, stdout: StringIO.new, stderr: StringIO.new)
    sharky.runtime = runtime

    assert_equal 0, sharky.run("ruby")
    assert_equal "yes", sharky.env["SHARKY_REPL_STATE"]
    assert_equal File.dirname(@tmp), sharky.cwd
  ensure
    ENV.delete("SHARKY_REPL_STATE")
  end
end
