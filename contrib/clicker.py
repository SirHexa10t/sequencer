#!/usr/bin/env python3

"""
hold/toggle clicks per second
--toggle (optional: change to Toggle mode; Hold mode is default) , --cps <number> (optional: change clicks per second) , --verbose (optional: print actions to terminal) , --key_press <key> (optional: replace click with a keyboard key)
"""

# import pyautogui  # pyautogui.click(pyautogui.position()) is too slow

from pynput import keyboard, mouse  # listens to keyboard events
from pynput.mouse import Button  # does clicks
import time  # handles waiting
import argparse  # handle arguments and help-message
import sys  # used for flushing some cached (debug) strings to terminal


parser = argparse.ArgumentParser()
parser.add_argument('--toggle', action='store_true', default=False, help='Use Toggle mode instead of Hold mode')
parser.add_argument('--verbose', action='store_true', help='Print actions in terminal')
parser.add_argument('--cps', action='store', default=20, type=float, help='How many clicks per second, default is 20')
parser.add_argument('--key_press', action='store', default='', type=str, help='Specify key repeat instead of click (example: \'f\')')
args = parser.parse_args()


activation_key = keyboard.Key.f9
exit_key = keyboard.Key.f8
# exit_key = keyboard.KeyCode(char='q')

is_clicking = False     # flag that gets raised when it's time to perform clicks/key-presses
is_running = True       # when this turns False, the program quits

# waiting time in seconds
clicking_interval = 1 / args.cps
keypress_sampling_interval = 0.05

RED = '\033[91m'
END = '\033[0m'
print(f'{RED}{"TOGGLE" if args.toggle else "HOLD"}{END}-clicker: '
      f'press {RED}{activation_key}{END} to do {RED}{args.cps} clicks/s{END}. '
      f'Quit with key: {RED}{exit_key}{END}')

def on_press(key):
    global is_clicking
    if not args.toggle and key == activation_key:
        is_clicking = True  # HOLD mode - clicking while pressed
    

def on_release(key):
    global is_clicking
    if key == exit_key:  # quit program
        global is_running
        is_running = False
        return  # not necessary, but to be sure...

    if key == activation_key:  # relevant key event
        if not args.toggle:
            is_clicking = False  # HOLD mode - release means no clicking
        else:  # release key in toggle mode = opposite of previous clicking state
            is_clicking = not is_clicking

if args.key_press:
    keyboard_controller = keyboard.Controller()
else:
    mouse_controller = mouse.Controller()
with keyboard.Listener(on_press=on_press, on_release=on_release) as listener:
    if args.verbose:
        clicks_done = 0
        is_last_print_wait = False
    while is_running:
        if is_clicking:
            start_time = time.time()
            # do the click
            if args.key_press:
                keyboard_controller.press(args.key_press)
                time.sleep(0.001)
                keyboard_controller.release(args.key_press)
            else:
                mouse_controller.click(Button.left, 1)

            elapsed_time = time.time() - start_time
            remaining_time = clicking_interval - elapsed_time

            if args.verbose:
                clicks_done += 1
                print(f"Click number {clicks_done}. Cycle is {clicking_interval}s ; waiting time left: {remaining_time}")
                is_last_print_wait = False
            if remaining_time > 0:  # sleep, maybe
                time.sleep(remaining_time)
        else:
            if args.verbose:
                if is_last_print_wait:
                    print('|', end="")  # print without newline at the end
                    sys.stdout.flush()
                else:
                    print(f'Waiting {keypress_sampling_interval}s. Every wait-cycle adds another char')
                is_last_print_wait = True
            time.sleep(keypress_sampling_interval)  # sleep
