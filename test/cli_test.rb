require_relative "test_helper"

class CliTest < Minitest::Test
  def test_prints_command_output
    out, err, status = run_sharky("-c", "echo hi >&2")
    assert status.success?
    assert_equal "hi\n", err
    assert_equal "", out
  end
end
