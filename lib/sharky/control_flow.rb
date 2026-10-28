# frozen_string_literal: true

module Sharky
  module ControlFlow
    class Base < StandardError
      attr_accessor :levels

      def initialize(levels = 1)
        @levels = [levels.to_i, 1].max
        super("control flow")
      end
    end

    class Break < Base
    end

    class Continue < Base
    end

    class Return < Base
      attr_reader :status

      def initialize(status = 0, levels = 1)
        @status = status.to_i
        super(levels)
      end
    end
  end
end
