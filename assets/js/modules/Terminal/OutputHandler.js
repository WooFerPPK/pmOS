/**
 * Represents a handler for terminal output.
 */
export class OutputHandler {
    /**
     * Initializes a new instance of the OutputHandler class.
     * 
     * @param {HTMLElement} outputElement - The element where the output will be displayed.
     * @param {Object} terminal - The terminal instance.
     */
    constructor(outputElement, terminal) {
        this.outputElement = outputElement;
        this.terminal = terminal;
        this.typeSpeed = 5;
        this.shouldType = true;
    }

    /**
     * Displays the startup message in the terminal.
     */
    displayStartupMessage() {
        const message = `
        Welcome to pmOS by Paul Moscuzza.<br/><br/>
        -> Type 'help' to get started.<br/>
        -> 'ctrl-c' to pause.<br/>
        -> 'ctrl-l' to clear the screen.<br/><br/>
        `;
        this.outputElement.innerHTML = message;
    }

    /**
     * Appends the executed command to the output display.
     * 
     * @param {string} command - The executed command.
     */
    appendCommandToOutput(command) {
        const commandDiv = document.createElement('div');
        commandDiv.textContent = '> ' + command;
        this.outputElement.appendChild(commandDiv);
    }

    /**
     * Extracts a full HTML element from a string starting from a specific index.
     * 
     * @param {string} str - The string containing HTML.
     * @param {number} startingIndex - The starting index of the tag.
     * @returns {Object} The extracted HTML element details.
     */
    extractFullHtmlElement(str, startingIndex) {
        const openTagEndIndex = str.indexOf('>', startingIndex);
        const tagDetails = str.substring(startingIndex + 1, openTagEndIndex).split(/\s+/);
        const tagName = tagDetails[0];
        const attributes = tagDetails.slice(1).join(' ');
        const closingTag = `</${tagName}>`;
        const closingTagIndex = str.indexOf(closingTag, openTagEndIndex);
        
        return {
            tagName,
            attributes,
            content: str.substring(openTagEndIndex + 1, closingTagIndex),
            endIndex: closingTagIndex + closingTag.length
        };
    }

    /**
     * Displays the output for a given command.
     * 
     * @param {string} command - The executed command.
     * @param {string|Promise} resultOrPromise - The output or a promise resolving to the output.
     */
    displayOutput(command, resultOrPromise) {
        this.shouldType = true;
        this.terminal.isOutputRunning = true;
        this.terminal.inputHandler.inputElement.contentEditable = 'false';
        this.appendCommandToOutput(command);
        
        if (resultOrPromise instanceof Promise) {
            resultOrPromise.then(result => {
                this.typeOutput(result);
            }).catch(err => {
                this.typeOutput(`Error: ${err.message}`, false);
            });
        } else {
            this.typeOutput(resultOrPromise);
        }
    }

    /**
     * Types the output in a typewriter fashion.
     * 
     * @param {string} result - The output to type out.
     */
    typeOutput(result) {
        let index = 0;
        let processingInnerContent = false;

        const processOutput = () => {
            if (!this.shouldType) return;
            if (processingInnerContent) {
                this.requestNextFrame(processOutput);
                return;
            }

            if (index < result.length) {
                const currentChar = result[index];
                if (currentChar === '<' && !processingInnerContent) {
                    processingInnerContent = true;

                    this.handleHtmlElement(result, index, (endIndex) => {
                        index = endIndex;
                        processingInnerContent = false;
                        this.requestNextFrame(processOutput);
                    });

                } else {
                    index += this.appendTextNode(currentChar, result, index);
                    this.requestNextFrame(processOutput);
                }
            } else {
                this.finishOutput();
            }
        };

        this.requestNextFrame(processOutput);
    }

    /**
     * Handles a specific HTML element and types its content.
     * 
     * @param {string} result - The output containing the HTML.
     * @param {number} index - The starting index of the HTML element.
     * @param {Function} callback - The callback function to be called after processing.
     */
    handleHtmlElement(result, index, callback) {
        const fullHtmlElement = this.extractFullHtmlElement(result, index);
        const { tagName, attributes, content, endIndex } = fullHtmlElement;

        const openingTag = `<${tagName} ${attributes}>`;
        const closingTag = `</${tagName}>`;

        const tagElement = this.createHtmlTagElement(openingTag);

        this.appendElement(tagElement);

        this.typeInnerContent(content, tagElement, () => {
            this.appendClosingTag(closingTag);
            callback(endIndex);
        });
    }

    /**
     * Types the inner content of an HTML element.
     * 
     * @param {string} content - The inner content of the HTML element.
     * @param {HTMLElement} tagElement - The HTML element itself.
     * @param {Function} callback - The callback function to be called after typing.
     */
    typeInnerContent(content, tagElement, callback) {
        let innerIndex = 0;

        const typeContent = () => {
            if (!this.shouldType) return;
            
            if (this.typeSpeed >= 0 && innerIndex < content.length && this.terminal.isOutputRunning) {
                const char = content[innerIndex];
                this.appendCharToElement(char, tagElement);
                innerIndex++;
                this.requestNextFrame(typeContent);
            } else if (this.typeSpeed < 0 && innerIndex < content.length) {
                const groupSize = Math.abs(this.typeSpeed);
                let groupedText = '';

                for (let i = 0; i < groupSize && innerIndex + i < content.length; i++) {
                    groupedText += content[innerIndex + i];
                }

                this.appendCharToElement(groupedText, tagElement);
                innerIndex += groupSize;
                this.requestNextFrame(typeContent);
            } else {
                callback();
            }
            
            this.scrollTerminalToBottom();
        };

        typeContent();
    }

    /**
     * Requests the next frame for the typing animation.
     * 
     * @param {Function} callback - The callback function to be called on the next frame.
     */
    requestNextFrame(callback) {
        if (this.typeSpeed > 0) {
            setTimeout(() => {
                requestAnimationFrame(callback);
            }, this.typeSpeed);
        } else {
            requestAnimationFrame(callback);
        }
        this.scrollTerminalToBottom();
    }

    setTypeSpeed(speed) {
        this.typeSpeed = speed;
    }

    /**
     * Increases the typing speed. 
     * If the speed is above 10, it reduces by 10.
     * If the speed is 10 or below but above 0, it sets the speed to -1.
     * For negative speeds, it decreases the speed (makes it faster).
     */
    speedUp() {
        if (this.typeSpeed > 10) {
            this.typeSpeed -= 10;
        } else if (this.typeSpeed <= 10 && this.typeSpeed > 0) {
            this.typeSpeed = -1;
        } else {
            this.typeSpeed--;
        }
    }

    /**
     * Decreases the typing speed (making it slower). 
     * If the speed is negative, it increases the speed (makes it slower).
     * If the speed becomes 0 from a negative value, it sets the speed to 10.
     * For positive speeds, it increases by 10.
     */
    slowDown() {
        if (this.typeSpeed < 0) {
            this.typeSpeed++;
            if (this.typeSpeed === 0) {
                this.typeSpeed = 10;
            }
        } else {
            this.typeSpeed += 10;
        }
    }

    /**
     * Appends a text node to the output element.
     * If the typing speed is positive, it appends one character.
     * If negative, it appends a group of characters at once.
     * 
     * @param {string} char - The current character.
     * @param {string} result - The full string being processed.
     * @param {number} currentIndex - The index of the current character in the string.
     * @returns {number} The number of characters processed.
     */
    appendTextNode(char, result, currentIndex) {
        if (this.typeSpeed >= 0) {
            const textNode = document.createTextNode(char);
            this.outputElement.appendChild(textNode);
            return 1;
        } else {
            const groupSize = Math.abs(this.typeSpeed);
            let groupedText = char;
    
            if (char === '<') {
                let tagEndIndex = result.indexOf('>', currentIndex);
                if (tagEndIndex !== -1) {
                    groupedText = result.substring(currentIndex, tagEndIndex + 1);
                }
            } else {
                for (let i = 1; i < groupSize && currentIndex + i < result.length; i++) {
                    if (result[currentIndex + i] === '<') {
                        break;
                    }
                    groupedText += result[currentIndex + i];
                }
            }
    
            const textNode = document.createTextNode(groupedText);
            this.outputElement.appendChild(textNode);
            return groupedText.length;
        }
    }
    
    /**
     * Appends an element to the output.
     * 
     * @param {HTMLElement} element - The element to append.
     */
    appendElement(element) {
        this.outputElement.appendChild(element);
    }

    /**
     * Creates an HTML element from a given opening tag string.
     * 
     * @param {string} openingTag - The opening tag string.
     * @returns {HTMLElement} The created HTML element.
     */
    createHtmlTagElement(openingTag) {
        const tempDiv = document.createElement('div');
        tempDiv.innerHTML = openingTag;
        return tempDiv.firstChild;
    }

    /**
     * Appends a character to a given HTML element.
     * 
     * @param {string} char - The character to append.
     * @param {HTMLElement} element - The element to which the character should be appended.
     */
    appendCharToElement(char, element) {
        element.appendChild(document.createTextNode(char));
    }

    /**
     * Appends a closing tag to the output.
     * 
     * @param {string} closingTag - The closing tag string.
     */
    appendClosingTag(closingTag) {
        this.outputElement.innerHTML += closingTag;
    }

    /**
     * Completes the current output by interrupting any ongoing typing and appending a prompt node.
     */
    finishOutput() {
        this.interruptOutput();
        const promptNode = document.createElement('div');
        this.outputElement.appendChild(promptNode);
        this.scrollTerminalToBottom();
    }

    /**
     * Scrolls the terminal UI to the bottom, ensuring the latest output is visible.
     */
    scrollTerminalToBottom() {
        this.terminal.terminalUI.terminalElement.scrollTop = this.terminal.terminalUI.terminalElement.scrollHeight;
    }

    /**
     * Stops any ongoing typing and resets the input field's interactivity.
     */
    interruptOutput() {
        this.shouldType = false;
        this.terminal.isOutputRunning = false;
        this.terminal.inputHandler.inputElement.contentEditable = 'true';
        this.terminal.inputHandler.inputElement.focus();
        this.terminal.terminalUI.terminalElement.scrollTop = this.terminal.terminalUI.terminalElement.scrollHeight;
    }
      
    /**
     * Clears all current output from the terminal.
     */
    clearOutput() {
        this.shouldType = false;
        this.outputElement.innerHTML = '';
    }
}