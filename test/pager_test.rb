# frozen_string_literal: true

require_relative "test_helper"
require "sharky"

class PagerTest < Minitest::Test
  class FakeTerminal
    attr_reader :input, :output

    def initialize(input, output)
      @input = input
      @output = output
    end

    def tty?
      true
    end

    def width(text)
      text.to_s.length
    end

    def size
      [80, 24]
    end
  end

  def test_filter_is_case_insensitive_substring
    pager = Sharky::Pager.new(terminal: nil, theme: Sharky::Theme.new)
    items = [
      Sharky::Completion::Candidate.new(value: "git push"),
      Sharky::Completion::Candidate.new(value: "ls")
    ]
    assert_equal ["git push"], pager.filter(items, "GP").map(&:value)
  end

  def test_select_filters_and_accepts
    reader, writer = IO.pipe
    output = StringIO.new
    pager = Sharky::Pager.new(terminal: FakeTerminal.new(reader, output), theme: Sharky::Theme.new)
    items = [
      Sharky::Completion::Candidate.new(value: "ls", description: "executable"),
      Sharky::Completion::Candidate.new(value: "git push", description: "alias")
    ]
    writer.write("gp\r")

    selected = pager.select(items, title: "Complete")

    assert_equal "git push", selected.value
    assert_includes output.string, "Complete"
  ensure
    writer&.close unless writer&.closed?
    reader&.close unless reader&.closed?
  end

  def test_escape_cancels
    reader, writer = IO.pipe
    output = StringIO.new
    pager = Sharky::Pager.new(terminal: FakeTerminal.new(reader, output), theme: Sharky::Theme.new)
    writer.write("\e")

    assert_nil pager.select([Sharky::Completion::Candidate.new(value: "ls")], title: "Complete")
    assert_includes output.string, "\e[J"
  ensure
    writer&.close unless writer&.closed?
    reader&.close unless reader&.closed?
  end

  def test_empty_selection_returns_nil
    pager = Sharky::Pager.new(terminal: nil, theme: Sharky::Theme.new)
    assert_nil pager.select([], title: "Complete")
  end
end
