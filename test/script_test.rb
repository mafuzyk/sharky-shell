require_relative "test_helper"

class ScriptTest < Minitest::Test
  def test_ruby_script_runs_with_api_and_exit_status
    path = File.join(@tmp, "script.rb")
    File.write(path, "puts 'script ok'\nexit Sharky.sh('false')\n")
    out, err, status = run_sharky(path)
    assert_equal 1, status.exitstatus, err
    assert_equal "script ok\n", out
  end

  def test_capture_returns_command_output
    path = File.join(@tmp, "script.rb")
    File.write(path, "print Sharky.capture('printf cap')\n")
    out, _err, status = run_sharky(path)
    assert_equal 0, status.exitstatus
    assert_equal "cap", out
  end

  def test_script_receives_arguments
    path = File.join(@tmp, "script.rb")
    File.write(path, "print ARGV.join('|')\n")
    out, err, status = run_sharky(path, "one", "two")
    assert_equal 0, status.exitstatus, err
    assert_equal "one|two", out
  end
end
