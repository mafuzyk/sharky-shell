require_relative "test_helper"
require "sharky"
require "stringio"

class PipelineTest < Minitest::Test
  def setup
    super
    @out = StringIO.new
    @err = StringIO.new
    @shell = Sharky::Shell.new(env: { "PATH" => "/usr/bin:/bin" }, cwd: @tmp, stdout: @out, stderr: @err, stdin: StringIO.new)
  end

  def test_pipeline
    assert_equal 0, @shell.run("printf 'a b\n' | wc -w")
    assert_equal "2\n", @out.string
  end

  def test_redirects
    file = File.join(@tmp, "out.txt")
    assert_equal 0, @shell.run("printf hi > #{file}")
    assert_equal "hi", File.read(file)
    assert_equal 0, @shell.run("printf hi >> #{file}")
    assert_equal "hihi", File.read(file)
  end

  def test_redirects_are_relative_to_shell_cwd
    assert_equal 0, @shell.run("printf hi > relative.txt")
    assert_equal "hi", File.read(File.join(@tmp, "relative.txt"))
  ensure
    FileUtils.rm_f(File.join(ROOT, "relative.txt"))
  end

  def test_pipeline_components_run_in_shell_cwd
    assert_equal 0, @shell.run("/usr/bin/pwd | cat")
    assert_equal "#{@tmp}\n", @out.string
  end

  def test_and_or_and_list
    assert_equal 0, @shell.run("false && printf no; true || printf no; printf yes")
    assert_equal "yes", @out.string
  end

  def test_subshell_does_not_change_parent
    assert_equal 0, @shell.run("(cd /tmp; pwd)")
    assert_equal @tmp, @shell.cwd
    assert_equal "/tmp\n", @out.string
  end

  def test_background_job_is_recorded
    assert_equal 0, @shell.run("sleep 0.2 &")
    assert_equal 1, @shell.jobs.all.size
    assert_equal 0, @shell.run("jobs")
    assert_match(/sleep 0\.2/, @out.string)
  end

  def test_input_redirect
    file = File.join(@tmp, "in.txt")
    File.write(file, "hello")
    assert_equal 0, @shell.run("wc -c < #{file}")
    assert_equal "5\n", @out.string
  end

  def test_group_input_redirect_reaches_external_commands
    File.write(File.join(@tmp, "input.txt"), "hello")

    assert_equal 0, @shell.run('{ wc -c; } < input.txt', compound: true)
    assert_equal "5\n", @out.string
  end

  def test_if_input_redirect_reaches_external_commands
    File.write(File.join(@tmp, "input.txt"), "hello")

    assert_equal 0, @shell.run('if true; then wc -c; fi < input.txt', compound: true)
    assert_equal "5\n", @out.string
  end

  def test_stderr_and_both_redirects
    file = File.join(@tmp, "both.txt")
    assert_equal 2, @shell.run("ls /definitely-not-here 2>&1")
    refute_empty @out.string

    @out.truncate(0)
    @out.rewind
    assert_equal 0, @shell.run("printf mixed &> #{file}")
    assert_equal "mixed", File.read(file)
  end
end
