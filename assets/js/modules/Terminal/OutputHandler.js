export class OutputHandler {
    constructor(outputElement, terminal) {
        this.outputElement = outputElement;
        this.terminal = terminal;
        this.typeSpeed = 5;
        this.shouldType = true;
    }

    displayStartupMessage() {
        const message = `
        Welcome to pmOS by Paul Moscuzza.<br/><br/>
        -> Type 'help' to get started.<br/>
        -> 'ctrl-c' to pause.<br/>
        -> 'ctrl-l' to clear the screen.<br/><br/>
        `;
        this.outputElement.innerHTML = message;
    }

    appendCommandToOutput(command) {
        const commandDiv = document.createElement('div');
        commandDiv.textContent = '> ' + command;
        this.outputElement.appendChild(commandDiv);
    }
    
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
    
    displayOutput(command, resultOrPromise) {
        this.shouldType = true;
        this.terminal.isOutputRunning = true;
        this.terminal.inputHandler.inputElement.contentEditable = "false";
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
                let groupedText = "";

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

    requestNextFrame(callback) {
        if (this.typeSpeed > 0) {
            setTimeout(() => {
                requestAnimationFrame(callback);
            }, this.typeSpeed);
        } else {
            requestAnimationFrame(callback);
        }
        this.scrollTerminalToBottom()
    }

    setTypeSpeed(speed) {
        this.typeSpeed = speed;
    }

    speedUp() {
        if (this.typeSpeed > 10) {
            this.typeSpeed -= 10;
        } else if (this.typeSpeed <= 10 && this.typeSpeed > 0) {
            this.typeSpeed = -1;
        } else {
            this.typeSpeed--;
        }
    }

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
    
    appendElement(element) {
        this.outputElement.appendChild(element);
    }

    createHtmlTagElement(openingTag) {
        const tempDiv = document.createElement('div');
        tempDiv.innerHTML = openingTag;
        return tempDiv.firstChild;
    }

    appendCharToElement(char, element) {
        element.appendChild(document.createTextNode(char));
    }

    appendClosingTag(closingTag) {
        this.outputElement.innerHTML += closingTag;
    }

    finishOutput() {
        this.interruptOutput();
        const promptNode = document.createElement('div');
        this.outputElement.appendChild(promptNode);
        this.scrollTerminalToBottom();
    }

    scrollTerminalToBottom() {
        this.terminal.terminalUI.terminalElement.scrollTop = this.terminal.terminalUI.terminalElement.scrollHeight;
    }
    
    interruptOutput() {
        this.shouldType = false;
        this.terminal.isOutputRunning = false;
        this.terminal.inputHandler.inputElement.contentEditable = "true";
        this.terminal.inputHandler.inputElement.focus();
        this.terminal.terminalUI.terminalElement.scrollTop = this.terminal.terminalUI.terminalElement.scrollHeight;
    }      

    clearOutput() {
        this.shouldType = false;
        this.outputElement.innerHTML = '';
    }
}