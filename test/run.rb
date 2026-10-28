#!/usr/bin/env ruby
# Runs every test file in one process.
$LOAD_PATH.unshift(File.expand_path("../lib", __dir__))
Dir[File.join(__dir__, "*_test.rb")].sort.each { |file| require file }
