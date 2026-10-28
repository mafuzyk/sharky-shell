# frozen_string_literal: true

module Sharky
  module Functions
    module_function

    def load(name, runtime)
      key = name.to_s
      existing = runtime.commands[key] || runtime.commands[key.to_sym]
      return existing if existing
      return nil if key.empty? || key.include?("/") || runtime.config_dir.nil?

      path = File.join(runtime.config_dir, "functions", "#{key}.rb")
      return nil unless File.file?(path)

      previous = Sharky.runtime
      Sharky.runtime = runtime
      begin
        Kernel.load(path)
      ensure
        Sharky.runtime = previous
      end
      runtime.commands[key] || runtime.commands[key.to_sym]
    end
  end
end
