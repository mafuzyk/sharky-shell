require_relative "test_helper"

class VersionTest < Minitest::Test
  def test_version_constant
    require "sharky/version"
    assert_equal "0.1.0", Sharky::VERSION
  end

  def test_bin_prints_version
    out, _err, status = run_sharky("--version")
    assert status.success?
    assert_equal "sharky 0.1.0\n", out
  end
end
