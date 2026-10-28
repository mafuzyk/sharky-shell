require_relative "test_helper"

class TrapTest < Minitest::Test
  def script(name, content)
    path = File.join(@tmp, name)
    File.write(path, content)
    path
  end

  def test_exit_trap_runs_after_script
    path = script("trap.sh", "trap 'echo bye' EXIT\necho hi\n")
    out, err, status = run_sharky(path)

    assert_equal 0, status.exitstatus
    assert_equal "hi\nbye\n", out
    assert_empty err
  end

  def test_exit_trap_preserves_status
    path = script("trap-status.sh", "trap 'echo bye' EXIT\nexit 5\n")
    out, _err, status = run_sharky(path)

    assert_equal 5, status.exitstatus
    assert_equal "bye\n", out
  end

  def test_trap_remove
    path = script("trap-remove.sh", "trap 'echo bye' EXIT\ntrap - EXIT\necho hi\n")
    out, err, status = run_sharky(path)

    assert_equal 0, status.exitstatus
    assert_equal "hi\n", out
    assert_empty err
  end

  def test_trap_print_and_list
    out, err, status = run_sharky("-c", "trap 'echo bye' EXIT; trap")
    assert_equal 0, status.exitstatus
    assert_includes out, "echo bye"
    assert_includes out, "EXIT"
    assert_empty err

    out, _err, status = run_sharky("-c", "trap -l")
    assert_equal 0, status.exitstatus
    assert_includes out, "EXIT"
  end

  def test_unknown_signal_returns_one
    _out, err, status = run_sharky("-c", "trap 'echo x' BOGUS")
    assert_equal 1, status.exitstatus
    assert_match(/invalid signal/i, err)
  end
end
