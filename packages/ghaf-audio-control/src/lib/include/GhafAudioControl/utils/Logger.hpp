/*
 * Copyright 2022-2024 TII (SSRC) and the Ghaf contributors
 * SPDX-License-Identifier: Apache-2.0
 */

#pragma once

#include <string_view>

namespace ghaf::AudioControl
{

class Logger
{
public:
    static void debug(std::string_view message);
    static void error(std::string_view message);
    static void info(std::string_view message);

private:
    static void log(std::string_view message, std::string_view logLevel);

    Logger();
};

} // namespace ghaf::AudioControl
